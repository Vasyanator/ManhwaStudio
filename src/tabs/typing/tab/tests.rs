// Unit tests for the typing tab; `super` resolves to the `tab` module.
use super::*;

#[test]
fn flatten_composites_raster_from_disk_fallback() {
    // Disk-fallback path (no snapshot in the job): rasters are read from `layers.json`, including the
    // migrated layout (committed-only page reached via the per-page fallback).
    use crate::models::layer_model::persist;
    let dir = std::env::temp_dir().join(format!("typ_flat_disk_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let layers = dir.join("layers");
    std::fs::create_dir_all(&layers).unwrap();
    let base = dir.join("page.png");
    image::save_buffer(
        &base,
        &vec![0u8; 20 * 20 * 4],
        20,
        20,
        image::ColorType::Rgba8,
    )
    .unwrap();
    let red = ColorImage::filled([10, 10], Color32::from_rgba_unmultiplied(255, 0, 0, 255));
    persist::add_page_raster(
        &layers,
        None,
        0,
        "r0",
        "R",
        true,
        1.0,
        crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        &red,
    )
    .unwrap();
    let job = TypingExportPageJob {
        page_idx: 0,
        page_path: base,
        output_path: dir.join("out.png"),
        clean_overlay_path: None,
        clean_overlay_rgba: None,
        overlays: Vec::new(),
        rasters: Vec::new(), // force the disk-read path
        mask: None,
        export_format: TypingExportFormat::Png,
        layers_primary_dir: Some(layers.clone()),
        layers_fallback_dir: None,
    };
    let (rgba, w, h) = flatten_typing_export_page_rgba(&job).unwrap();
    assert_eq!([w, h], [20, 20]);
    let center = (10 * 20 + 10) * 4;
    assert_eq!(
        &rgba[center..center + 4],
        &[255, 0, 0, 255],
        "disk raster composited at center"
    );

    // Migrated layout: primary=unsaved (manifest exists, lacks page 0), raster on committed page 0.
    let committed = dir.join("committed");
    let unsaved = dir.join("unsaved");
    std::fs::create_dir_all(&committed).unwrap();
    std::fs::create_dir_all(&unsaved).unwrap();
    persist::add_page_raster(
        &committed,
        None,
        0,
        "rc",
        "R",
        true,
        1.0,
        crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        &red,
    )
    .unwrap();
    persist::add_page_raster(
        &unsaved,
        None,
        5,
        "rs",
        "R",
        true,
        1.0,
        crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        &red,
    )
    .unwrap();
    let base2 = dir.join("page2.png");
    image::save_buffer(
        &base2,
        &vec![0u8; 20 * 20 * 4],
        20,
        20,
        image::ColorType::Rgba8,
    )
    .unwrap();
    let job2 = TypingExportPageJob {
        page_idx: 0,
        page_path: base2,
        output_path: dir.join("out2.png"),
        clean_overlay_path: None,
        clean_overlay_rgba: None,
        overlays: Vec::new(),
        rasters: Vec::new(),
        mask: None,
        export_format: TypingExportFormat::Png,
        layers_primary_dir: Some(unsaved.clone()),
        layers_fallback_dir: Some(committed.clone()),
    };
    let (rgba2, _, _) = flatten_typing_export_page_rgba(&job2).unwrap();
    assert_eq!(
        &rgba2[center..center + 4],
        &[255, 0, 0, 255],
        "committed-only raster composited (migrated)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flatten_composites_raster_from_on_screen_snapshot() {
    // PRIMARY Bug B fix: the export composites the ON-SCREEN raster snapshot (`job.rasters`) even when
    // the disk dirs would yield NOTHING (no `layers.json` at all) — proving the bake no longer depends
    // on a disk re-read that can silently drop the raster.
    let dir = std::env::temp_dir().join(format!("typ_flat_snap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        panic!("could not create deferred-raster test directory: {err}");
    }
    let base = dir.join("page.png");
    image::save_buffer(
        &base,
        &vec![0u8; 20 * 20 * 4],
        20,
        20,
        image::ColorType::Rgba8,
    )
    .unwrap_or_else(|err| panic!("could not seed deferred-raster test layer: {err}"));
    // A 10x10 RED straight-alpha snapshot centered at (10,10), no disk dirs.
    let snap = TypingExportRasterSnapshot {
        visible: true,
        opacity: 1.0,
        transform: crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        rgba: [255, 0, 0, 255].repeat(10 * 10),
        size_px: [10, 10],
        band_z: 0,
        mask_clip_enabled: false,
    };
    let job = TypingExportPageJob {
        page_idx: 0,
        page_path: base,
        output_path: dir.join("out.png"),
        clean_overlay_path: None,
        clean_overlay_rgba: None,
        overlays: Vec::new(),
        rasters: vec![snap],
        mask: None,
        export_format: TypingExportFormat::Png,
        layers_primary_dir: None, // no disk source at all
        layers_fallback_dir: None,
    };
    let (rgba, w, h) = flatten_typing_export_page_rgba(&job).unwrap();
    assert_eq!([w, h], [20, 20]);
    let center = (10 * 20 + 10) * 4;
    assert_eq!(
        &rgba[center..center + 4],
        &[255, 0, 0, 255],
        "on-screen snapshot raster composited"
    );
    // A hidden snapshot is skipped.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flatten_clips_mask_clip_enabled_raster_in_export() {
    // ITEM B: a mask-clip-ENABLED raster must export CLIPPED — pixels over an inactive page mask are
    // absent (transparent), matching the on-screen `clipped_image`. An unclipped raster is unchanged.
    use crate::tabs::typing::mask::TypingMaskExportPage;
    let dir = std::env::temp_dir().join(format!("typ_flat_maskclip_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("page.png");
    // 20x20 OPAQUE black base (alpha 255), so a clipped raster reveals the base, not transparency.
    let base_px: Vec<u8> = (0..20 * 20).flat_map(|_| [0u8, 0, 0, 255]).collect();
    image::save_buffer(&base, &base_px, 20, 20, image::ColorType::Rgba8).unwrap();

    // A 10x10 RED raster centered at (10,10) → covers page px [5..15]x[5..15].
    let make_snap = |mask_clip: bool| TypingExportRasterSnapshot {
        visible: true,
        opacity: 1.0,
        transform: crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        rgba: [255, 0, 0, 255].repeat(10 * 10),
        size_px: [10, 10],
        band_z: 0,
        mask_clip_enabled: mask_clip,
    };
    // Page mask ACTIVE only on the LEFT half (x < 10) of the 20x20 page.
    let mask = TypingMaskExportPage {
        width: 20,
        height: 20,
        data: (0..20 * 20)
            .map(|i| if (i % 20) < 10 { 255 } else { 0 })
            .collect(),
    };
    let make_job = |snap: TypingExportRasterSnapshot, mask: Option<TypingMaskExportPage>| {
        TypingExportPageJob {
            page_idx: 0,
            page_path: base.clone(),
            output_path: dir.join("out.png"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: Vec::new(),
            rasters: vec![snap],
            mask,
            export_format: TypingExportFormat::Png,
            layers_primary_dir: None,
            layers_fallback_dir: None,
        }
    };

    // CLIPPED export: left-half page pixels keep the raster (red); right-half are clipped → base (black).
    let (rgba, _, _) =
        flatten_typing_export_page_rgba(&make_job(make_snap(true), Some(mask.clone()))).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * 20 + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    };
    assert_eq!(
        px(7, 10),
        [255, 0, 0, 255],
        "raster kept where mask is active (left half)"
    );
    assert_eq!(
        px(13, 10),
        [0, 0, 0, 255],
        "raster CLIPPED where mask is inactive (right half)"
    );

    // UNCLIPPED (mask_clip OFF): the same right-half pixel keeps the raster.
    let (rgba2, _, _) =
        flatten_typing_export_page_rgba(&make_job(make_snap(false), Some(mask))).unwrap();
    let i = (10 * 20 + 13) * 4;
    assert_eq!(
        &rgba2[i..i + 4],
        &[255, 0, 0, 255],
        "unclipped raster unchanged on the right half"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_char_budget_floors_at_min_and_grows_with_width() {
    let cp = 8.0; // representative char width px
    // At/below the min available width (5 chars fit) → exactly the min (5).
    assert_eq!(preview_char_budget(5.0 * cp, cp), 5, "5 chars fit → 5");
    assert_eq!(preview_char_budget(0.0, cp), 5, "no room → still min 5");
    assert_eq!(
        preview_char_budget(-50.0, cp),
        5,
        "negative (overhead > width) → min 5"
    );
    assert_eq!(
        preview_char_budget(3.0 * cp, cp),
        5,
        "only 3 fit but floor is 5"
    );
    // Grows by 1 per ~char_px wider.
    assert_eq!(preview_char_budget(6.0 * cp, cp), 6, "6 chars wide → 6");
    assert_eq!(
        preview_char_budget(6.0 * cp + cp / 2.0, cp),
        6,
        "partial char floors down"
    );
    assert_eq!(preview_char_budget(12.0 * cp, cp), 12, "12 chars wide → 12");
    // Degenerate inputs → min (helper guards non-finite available + non-positive char_px).
    assert_eq!(
        preview_char_budget(1000.0, 0.0),
        5,
        "zero char width → min 5"
    );
    assert_eq!(
        preview_char_budget(f32::INFINITY, cp),
        5,
        "non-finite available → min 5"
    );
    assert_eq!(
        preview_char_budget(f32::NAN, cp),
        5,
        "NaN available → min 5"
    );
}

#[test]
fn text_preview_label_appends_dots_to_three_accounting_for_existing() {
    // First `max_chars` CHARACTERS (Unicode), trailing dot-equivalents brought to >= 3 (regular dot
    // = 1, ellipsis '…' = 3), accounting for what's already there. These use max_chars = 5 (the min).
    assert_eq!(
        text_preview_label("Привет мир", 5),
        "Приве...",
        "no trailing dots → append 3"
    );
    assert_eq!(
        text_preview_label("Да.", 5),
        "Да...",
        "1 existing dot → append 2"
    );
    assert_eq!(
        text_preview_label("Эй..", 5),
        "Эй...",
        "2 existing dots → append 1"
    );
    // "Стоп..." → first5 = "Стоп." (С,т,о,п,.), 1 trailing dot → append 2.
    assert_eq!(
        text_preview_label("Стоп...", 5),
        "Стоп...",
        "first-5 truncation keeps one dot → append 2"
    );
    // Ellipsis char counts as 3 → append none.
    assert_eq!(
        text_preview_label("Всё…", 5),
        "Всё…",
        "ellipsis = 3 → append none"
    );
    // "Хм….." → first5 = Х,м,…,.,. → trailing .,. then … = 1+1+3 = 5 → append none.
    assert_eq!(
        text_preview_label("Хм…..", 5),
        "Хм…..",
        "ellipsis + 2 dots → already >= 3"
    );
    // Short text (< 5 chars), not truncated, still gets dots.
    assert_eq!(text_preview_label("Да", 5), "Да...");
    // Empty (after trim) → empty preview (caller shows just "Текст").
    assert_eq!(text_preview_label("", 5), "");
    assert_eq!(
        text_preview_label("   ", 5),
        "",
        "whitespace-only trims to empty"
    );
    // Leading whitespace is trimmed before taking the first 5 chars.
    assert_eq!(text_preview_label("  Привет", 5), "Приве...");
    // Cyrillic char-boundary safety: exactly 5 chars taken, no byte-panic on multibyte text.
    let long = "Текстовая строка";
    assert!(long.chars().count() > 5);
    assert_eq!(text_preview_label(long, 5), "Текст...");
    // A 5-char prefix that is ALL dots stays as-is (>= 3).
    assert_eq!(text_preview_label(".....", 5), ".....");

    // Larger max_chars → more preview chars before the dots (wider panel). "Длинноеслово" has no
    // space in the first 10, so the prefix is exactly its first 10 chars.
    assert_eq!(
        text_preview_label("Длинноеслово", 10),
        "Длинноесло...",
        "first 10 chars + dots"
    );
    // A text SHORTER than max_chars still gets the dots.
    assert_eq!(
        text_preview_label("Привет", 10),
        "Привет...",
        "short-than-max still gets dots"
    );
    // Dot accounting still applies with a larger budget.
    assert_eq!(
        text_preview_label("Конец..", 10),
        "Конец...",
        "2 trailing dots → append 1"
    );
}

#[test]
fn order_unified_layer_rows_interleaves_by_z_overlay_above_raster_on_ties() {
    use TypingLayerRow::*;
    // Rows with band-Z; bool = raster_below_overlay (true for rasters).
    // overlay@5, raster@5 (tie → overlay above), raster@3, overlay@1.
    let rows = vec![
        (Overlay(0), 5, false),
        (Raster(0), 5, true),
        (Raster(1), 3, true),
        (Overlay(1), 1, false),
    ];
    // TOP-first (Z desc): overlay@5, raster@5 (overlay wins the tie → listed first), raster@3, overlay@1.
    assert_eq!(
        order_unified_layer_rows(rows),
        vec![Overlay(0), Raster(0), Raster(1), Overlay(1)]
    );

    // A raster strictly ABOVE a text (text can sit below a raster now): raster@7 first.
    let rows2 = vec![(Overlay(2), 2, false), (Raster(2), 7, true)];
    assert_eq!(order_unified_layer_rows(rows2), vec![Raster(2), Overlay(2)]);

    // Empty input → empty output.
    assert!(order_unified_layer_rows(Vec::new()).is_empty());
}

#[test]
fn unified_topmost_pointer_target_picks_by_z_overlay_wins_ties() {
    let t = TypingPointerTarget::Overlay;
    let r = TypingPointerTarget::Raster;
    let n = TypingPointerTarget::None;
    // Text above raster → text wins.
    assert_eq!(unified_topmost_pointer_target(Some(5), Some(2)), t);
    // Raster above text → raster wins (text can now sit BELOW a raster).
    assert_eq!(unified_topmost_pointer_target(Some(2), Some(5)), r);
    // Equal band-Z → overlay wins (text draws above a raster at the same band).
    assert_eq!(unified_topmost_pointer_target(Some(3), Some(3)), t);
    // Only one present → that one.
    assert_eq!(unified_topmost_pointer_target(Some(0), None), t);
    assert_eq!(unified_topmost_pointer_target(None, Some(0)), r);
    // Neither under the pointer → None.
    assert_eq!(unified_topmost_pointer_target(None, None), n);
}

#[test]
fn topmost_raster_target_skips_selected_and_picks_topmost() {
    // The normal-mode raster interaction creates the SELECTED raster's response unconditionally, so
    // the hit-test for the OTHER rasters must skip the selected idx (else egui gets a duplicate Id).
    // It must also pick the TOPMOST (last in bottom-to-top `entries`) when quads overlap.
    let image_rect = Rect::from_min_size(Pos2::new(0.0, 0.0), egui::vec2(1000.0, 1000.0));
    let quad = |cx: f32, cy: f32| -> [Pos2; 4] {
        [
            Pos2::new(cx - 20.0, cy - 20.0),
            Pos2::new(cx + 20.0, cy - 20.0),
            Pos2::new(cx + 20.0, cy + 20.0),
            Pos2::new(cx - 20.0, cy + 20.0),
        ]
    };
    // Two overlapping rasters at the same center: idx 0 (bottom), idx 1 (top).
    let entries = vec![
        (0usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0)),
        (1usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0)),
    ];
    let p = Some(Pos2::new(100.0, 100.0));

    // No exclusion → topmost (idx 1) wins.
    let t = topmost_raster_target(&entries, p, image_rect, None).expect("hit");
    assert_eq!(t.0, 1, "topmost (last) raster wins on overlap");

    // Exclude the selected top raster → the hit-test falls through to idx 0 (no duplicate Id).
    let t = topmost_raster_target(&entries, p, image_rect, Some(1)).expect("hit");
    assert_eq!(t.0, 0, "selected idx skipped, next raster targeted");

    // Pointer far outside every quad → no target.
    assert!(
        topmost_raster_target(&entries, Some(Pos2::new(900.0, 900.0)), image_rect, None).is_none()
    );

    // No pointer → no target.
    assert!(topmost_raster_target(&entries, None, image_rect, None).is_none());

    // Excluding the only raster under the pointer → no target.
    let single = vec![(5usize, quad(100.0, 100.0), Pos2::new(100.0, 100.0))];
    assert!(topmost_raster_target(&single, p, image_rect, Some(5)).is_none());
}

#[test]
fn color_image_to_rgba_round_trips_straight_alpha() {
    // BUG A: `color_image_to_rgba` must return STRAIGHT (un-premultiplied) alpha so it round-trips
    // through `ColorImage::from_rgba_unmultiplied`. With the old `to_array()` (premultiplied), white
    // (255,255,255,128) came back as (128,128,128,128) — graying antialiased stroke edges.
    let straight: Vec<u8> = vec![
        255, 255, 255, 128, 200, 100, 50, 64, 10, 20, 30, 255, 0, 0, 0, 0,
    ];
    let image = ColorImage::from_rgba_unmultiplied([4, 1], &straight);
    let out = color_image_to_rgba(&image);
    assert_eq!(out.len(), straight.len());
    // Alpha round-trips exactly; RGB is recovered within the unavoidable premultiply→u8→unpremultiply
    // quantization (≈255/alpha), which the OLD `to_array()` (premultiplied) would blow past entirely.
    for px in 0..4 {
        let a = straight[px * 4 + 3] as i32;
        assert_eq!(
            out[px * 4 + 3],
            straight[px * 4 + 3],
            "alpha exact at pixel {px}"
        );
        // Worst-case round-trip error ≈ ceil(255 / (2*alpha)).
        let tol = if a == 0 {
            0
        } else {
            ((255 + 2 * a - 1) / (2 * a)).max(1)
        };
        for ch in 0..3 {
            let (g, o) = (out[px * 4 + ch] as i32, straight[px * 4 + ch] as i32);
            // A fully-transparent pixel's RGB is undefined post-premult; skip it.
            if a == 0 {
                continue;
            }
            assert!(
                (g - o).abs() <= tol,
                "pixel {px} ch {ch}: round-tripped {g} != original {o} (±{tol}, alpha {a})"
            );
        }
    }
    // The CRITICAL guard: un-premultiplied white (255,255,255,128) must NOT come back grayed to ~128
    // (the old `to_array()` premultiplied bug). With the fix it stays white.
    assert!(
        out[0] >= 254 && out[1] >= 254 && out[2] >= 254,
        "white stays white, not premultiplied gray"
    );
}

#[test]
fn image_effects_fx_file_name_appends_fx_suffix() {
    assert_eq!(
        image_effects_fx_file_name("image_p0_1.png"),
        "image_p0_1_fx.png"
    );
    assert_eq!(image_effects_fx_file_name("photo.jpeg"), "photo_fx.jpeg");
    // Без расширения — по умолчанию png.
    assert_eq!(image_effects_fx_file_name("noext"), "noext_fx.png");
}

#[test]
fn raster_identity_deform_seed_is_a_valid_grid_over_the_affine_quad() {
    // Entering raster transform mode seeds an identity deform from the affine transform via
    // `default_deform_mesh_for_page` (the same fn `ensure_raster_deform_mesh` uses for a raster
    // with no deform). It must produce a valid cols×rows grid whose corners equal the affine quad.
    let page_size = [200, 100];
    let center = [100.0_f32, 50.0];
    let size = [40usize, 20];
    let mesh = default_deform_mesh_for_page(center, size, 1.0, 0.0, page_size);
    assert_eq!(mesh.cols, TEXT_OVERLAY_DEFORM_SURFACE_COLS);
    assert_eq!(mesh.rows, TEXT_OVERLAY_DEFORM_SURFACE_ROWS);
    assert_eq!(mesh.points_px.len(), mesh.cols * mesh.rows);
    // The 4 grid corners are the affine image quad corners (centered, unrotated, unit scale).
    let tl = mesh.point(0, 0);
    let br = mesh.point(mesh.cols - 1, mesh.rows - 1);
    assert!(
        (tl[0] - (center[0] - size[0] as f32 * 0.5)).abs() < 1e-2,
        "TL x = cx - w/2"
    );
    assert!(
        (tl[1] - (center[1] - size[1] as f32 * 0.5)).abs() < 1e-2,
        "TL y = cy - h/2"
    );
    assert!(
        (br[0] - (center[0] + size[0] as f32 * 0.5)).abs() < 1e-2,
        "BR x = cx + w/2"
    );
    assert!(
        (br[1] - (center[1] + size[1] as f32 * 0.5)).abs() < 1e-2,
        "BR y = cy + h/2"
    );
}

#[test]
fn perspective_corner_drag_moves_the_dragged_corner_fully() {
    // The raster perspective transform mode drags a mesh corner via `apply_perspective_corner_drag`
    // (shared with overlays): the dragged corner moves by the full delta; the opposite corner is
    // untouched.
    let page_size = [500, 500];
    let mesh = default_deform_mesh_for_page([250.0, 250.0], [100, 100], 1.0, 0.0, page_size);
    let tl_before = mesh.point(0, 0);
    let br_before = mesh.point(mesh.cols - 1, mesh.rows - 1);
    // Drag handle 0 (top-left) by (+10, +20) page px.
    let dragged = apply_perspective_corner_drag(&mesh, 0, [10.0, 20.0], page_size);
    let tl_after = dragged.point(0, 0);
    let br_after = dragged.point(dragged.cols - 1, dragged.rows - 1);
    assert!(
        (tl_after[0] - (tl_before[0] + 10.0)).abs() < 1e-3,
        "TL fully follows the drag x"
    );
    assert!(
        (tl_after[1] - (tl_before[1] + 20.0)).abs() < 1e-3,
        "TL fully follows the drag y"
    );
    assert!(
        (br_after[0] - br_before[0]).abs() < 1e-3,
        "opposite corner unaffected x"
    );
    assert!(
        (br_after[1] - br_before[1]).abs() < 1e-3,
        "opposite corner unaffected y"
    );
}

#[test]
fn deform_mesh_clamps_to_page_size_not_text_bitmap_size() {
    // Regression: a deformed text overlay's `DeformRec.points_px` are ABSOLUTE PAGE pixels. When
    // `sync_from_doc` re-materializes the runtime mesh after a drag-release round-trip through the
    // doc, it must clamp against the PAGE size, not the small text-bitmap size. Passing the bitmap
    // size collapses the full-page control points into a degenerate box near the origin, so the
    // deformed text vanishes on the first idle frame after release. This asserts the invariant the
    // fix restores: page-size construction preserves lower/right-page points; bitmap-size
    // construction collapses them.
    let cols = TEXT_OVERLAY_DEFORM_SURFACE_COLS;
    let rows = TEXT_OVERLAY_DEFORM_SURFACE_ROWS;
    let page_size = [800usize, 1200];
    // A grid placed in the lower-right region of the page — outside the small bitmap's clamp range.
    let (x0, x1) = (350.0_f32, 650.0);
    let (y0, y1) = (900.0_f32, 1100.0);
    let mut points_px = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        let tv = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let tu = col as f32 / (cols - 1) as f32;
            points_px.push([x0 + (x1 - x0) * tu, y0 + (y1 - y0) * tv]);
        }
    }
    let br = [points_px[cols * rows - 1][0], points_px[cols * rows - 1][1]];

    // Page-size construction: control points survive intact (all within [-0.9·side, 1.9·side]).
    let good = TypingOverlayDeformMesh::new(cols, rows, points_px.clone(), page_size)
        .expect("valid grid builds a mesh");
    let br_good = good.point(cols - 1, rows - 1);
    assert!((br_good[0] - br[0]).abs() < 1e-3, "page-size keeps BR x");
    assert!((br_good[1] - br[1]).abs() < 1e-3, "page-size keeps BR y");

    // Bitmap-size construction (the bug): the small size clamps the lower-right point away from its
    // true page position, proving the size argument must be the page size.
    let bitmap_size = [300usize, 120];
    let bad = TypingOverlayDeformMesh::new(cols, rows, points_px, bitmap_size)
        .expect("valid grid builds a mesh");
    let br_bad = bad.point(cols - 1, rows - 1);
    // 1.9 * 120 = 228 is the max clamped y; the true y (1100) is far above it.
    assert!(
        br_bad[1] < br[1] - 1.0,
        "bitmap-size collapses BR y (the bug the fix avoids)"
    );
}

#[test]
fn effects_json_array_emptiness_is_detected() {
    assert!(effects_json_array_is_empty(""));
    assert!(effects_json_array_is_empty("   "));
    assert!(effects_json_array_is_empty("[]"));
    assert!(!effects_json_array_is_empty(r#"[{"effect":"stroke"}]"#));
    // Некорректный JSON трактуем как «пусто», чтобы не падать на мусоре.
    assert!(effects_json_array_is_empty("not-json"));
}

#[test]
fn raster_selection_tracks_by_uid_across_a_reorder() {
    // FIX 2 (wrong-layer): `selected_raster_idx` / `transform_mode_raster_idx` /
    // `raster_drag_state.raster_idx` are POSITIONS into `raster_layers_by_page[page]`, which
    // `sync_from_doc` rebuilds in z-order on every reproject. After a raster reorder the SAME position
    // points at a DIFFERENT raster — so transform/delete would hit the wrong one. The remap at the end
    // of `sync_from_doc` must keep these tracking the SAME raster by uid.
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_rsel_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tf = crate::models::layer_model::manifest::TransformRec {
        cx: 1.0,
        cy: 1.0,
        rotation: 0.0,
        scale: 1.0,
    };
    let pic = ColorImage::filled([2, 2], Color32::WHITE);
    // Add order is bottom-to-top: r0 (bottom), r1 (top).
    persist::add_page_raster(&dir, None, 0, "r0", "Bottom", true, 1.0, tf, &pic).unwrap();
    persist::add_page_raster(&dir, None, 0, "r1", "Top", true, 1.0, tf, &pic).unwrap();

    let mut doc = LayerDoc::new();
    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();

    let mut layer = TypingTextOverlayLayer::default();
    layer.sync_from_doc(0, &doc);
    let rasters = &layer.raster_layers_by_page[&0];
    assert_eq!(rasters.len(), 2);
    // Projected bottom-to-top: index 0 == r0, index 1 == r1.
    let r0_pos = rasters.iter().position(|l| l.uid == "r0").unwrap();
    let r1_pos = rasters.iter().position(|l| l.uid == "r1").unwrap();
    assert_eq!(r0_pos, 0);

    // Select r0 (bottom), enter transform mode on it, and start a drag tracking it.
    layer.selected_raster_idx = Some(r0_pos);
    layer.selected_raster_page = Some(0);
    layer.transform_mode_raster_idx = Some(r0_pos);
    layer.raster_drag_state = Some(TypingRasterDragState {
        page_idx: 0,
        raster_idx: r0_pos,
        mode: TypingRasterDragMode::Move,
        pointer_start_scene: Pos2::ZERO,
        start_transform: tf,
        start_pointer_angle_rad: 0.0,
        start_mesh: None,
    });

    // Reorder r0 UP past r1 in the doc, then reproject.
    assert!(doc.reorder_node_one(0, "r0", true));
    layer.sync_from_doc(0, &doc);

    let rasters = &layer.raster_layers_by_page[&0];
    let r0_new = rasters.iter().position(|l| l.uid == "r0").unwrap();
    assert_ne!(
        r0_new, r0_pos,
        "the reorder actually moved r0 to a new position"
    );
    // All three trackers now point at r0's NEW position (the SAME raster), not the stale index.
    assert_eq!(
        layer.selected_raster_idx,
        Some(r0_new),
        "selection follows r0 by uid"
    );
    assert_eq!(
        layer.transform_mode_raster_idx,
        Some(r0_new),
        "transform mode follows r0 by uid"
    );
    assert_eq!(
        layer.raster_drag_state.as_ref().map(|d| d.raster_idx),
        Some(r0_new),
        "drag state follows r0 by uid"
    );
    // The stale position now holds r1 — proof a positional tracker would have retargeted.
    assert_eq!(rasters[r0_pos].uid, "r1");
    let _ = r1_pos;

    // A deleted raster clears the trackers instead of pointing at a neighbour.
    layer.selected_raster_idx = Some(r0_new);
    layer.selected_raster_page = Some(0);
    assert!(doc.remove_node(0, "r0"));
    layer.sync_from_doc(0, &doc);
    assert_eq!(
        layer.selected_raster_idx, None,
        "selection cleared when its raster is gone"
    );
    assert_eq!(
        layer.selected_raster_page, None,
        "selection page cleared in lock-step when its raster is gone"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn select_raster_sets_page_and_clear_resets_it_in_lockstep() {
    // The Ctrl+wheel / scale / arrow-nudge shortcuts run once per visible page and guard on
    // `selected_raster_page == Some(page_idx)`, so `select_raster` MUST record the page and every
    // deselect MUST clear both fields together (else one gesture rotates the same index on every
    // page). This locks the invariant without the GUI-coupled wheel handler.
    let mut layer = TypingTextOverlayLayer::default();
    assert_eq!(layer.selected_raster_idx, None);
    assert_eq!(layer.selected_raster_page, None);

    layer.select_raster(7, 2);
    assert_eq!(layer.selected_raster_idx, Some(2));
    assert_eq!(
        layer.selected_raster_page,
        Some(7),
        "select_raster records the page"
    );

    // Re-selecting on a different page moves the page in lock-step.
    layer.select_raster(4, 1);
    assert_eq!(layer.selected_raster_idx, Some(1));
    assert_eq!(layer.selected_raster_page, Some(4));

    layer.clear_selection();
    assert_eq!(layer.selected_raster_idx, None);
    assert_eq!(
        layer.selected_raster_page, None,
        "clear_selection resets the page alongside the index"
    );
}

#[test]
fn remove_raster_clears_page_when_it_empties_the_selected_index() {
    // Deleting the currently-selected raster: `shift_index_after_remove` sets
    // `selected_raster_idx = None` when the removed index equals the selection, and
    // `selected_raster_page` MUST follow (lock-step, per the `tab.rs` invariant). Guarded on the
    // selection's page so an index on another page is never shifted against this page's removal.
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_rrm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tf = crate::models::layer_model::manifest::TransformRec {
        cx: 1.0,
        cy: 1.0,
        rotation: 0.0,
        scale: 1.0,
    };
    let pic = ColorImage::filled([2, 2], Color32::WHITE);
    persist::add_page_raster(&dir, None, 0, "r0", "Bottom", true, 1.0, tf, &pic).unwrap();
    persist::add_page_raster(&dir, None, 0, "r1", "Top", true, 1.0, tf, &pic).unwrap();

    let mut doc = LayerDoc::new();
    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();

    let mut layer = TypingTextOverlayLayer::default();
    layer.sync_from_doc(0, &doc);
    assert_eq!(layer.raster_layers_by_page[&0].len(), 2);

    // Select the raster at index 1 on page 0, then delete that exact raster.
    layer.select_raster(0, 1);
    assert_eq!(layer.selected_raster_idx, Some(1));
    assert_eq!(layer.selected_raster_page, Some(0));
    layer.remove_raster(0, 1);
    assert_eq!(
        layer.selected_raster_idx, None,
        "removing the selected raster empties the index"
    );
    assert_eq!(
        layer.selected_raster_page, None,
        "and the page is cleared in lock-step"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sync_from_doc_materializes_text_runtimes_for_a_migrated_chapter() {
    // LIVE BUG: after the eager migration `text_info.json` is retired (.bak), so the legacy disk
    // loader populates NO `self.overlays`. `sync_from_doc` must MATERIALIZE a text runtime from each
    // doc Text node that has no local runtime (reconcile-OR-CREATE), else the typing tab shows no
    // text while PS + the doc carry it. A second sync must NOT duplicate them (reconcile path).
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_migtext_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Seed two inline v3 text nodes on page 0 with real rendered PNGs (no text_info.json — migrated).
    let seed_text = |uid: &str, cx: f32, cy: f32| -> persist::TextPayloadOut {
        let img = ColorImage::filled([4, 3], Color32::GREEN);
        let file = persist::write_text_image(&dir, 0, uid, &img).unwrap();
        persist::TextPayloadOut {
            uid: uid.into(),
            name: uid.into(),
            z: 1,
            layer_idx: 2,
            pinned: false,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            pinned_by_group: false,
            payload_uid: uid.into(),
            render_data: json!({ "text": uid }),
            is_image: false,
            transform: crate::models::layer_model::manifest::TransformRec {
                cx,
                cy,
                rotation: 0.0,
                scale: 1.0,
            },
            deform: None,
            rendered_file: Some(file),
            mask_clip: None,
        }
    };
    persist::write_page_text_payload(
        &dir,
        None,
        0,
        &[seed_text("ta", 10.0, 20.0), seed_text("tb", 30.0, 40.0)],
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();
    assert_eq!(
        doc.page(0)
            .unwrap()
            .nodes
            .iter()
            .filter(|n| n.is_text())
            .count(),
        2,
        "doc loaded both text nodes"
    );

    // Migrated-chapter state: NO local overlay runtimes.
    let mut layer = TypingTextOverlayLayer::default();
    assert!(layer.overlays.is_empty());

    layer.sync_from_doc(0, &doc);

    // Both text nodes materialized as runtimes with correct projected fields.
    assert_eq!(
        layer.overlays.len(),
        2,
        "sync_from_doc created a runtime per doc text node"
    );
    let ta = layer
        .overlays
        .iter()
        .find(|o| o.uid == "ta")
        .expect("ta runtime");
    assert_eq!(ta.kind, TypingOverlayKind::Text);
    assert_eq!(ta.page_idx, 0);
    assert_eq!(ta.center_page_px, [10.0, 20.0]);
    assert!((ta.angle_deg - 0.0).abs() < 1e-6);
    assert!((ta.user_scale - 1.0).abs() < 1e-6);
    assert_eq!(ta.layer_idx, 2, "text-group axis carried from the node");
    assert_eq!(ta.size_px, [4, 3], "doc image projected");
    assert_eq!(
        ta.source_rgba.len(),
        4 * 3 * 4,
        "rgba populated from the doc image"
    );
    assert_eq!(
        ta.file_name,
        persist::text_image_file_name(0, "ta"),
        "deterministic rendered-PNG name (round-trips with the doc flush)"
    );
    assert!(
        ta.texture.is_none() && ta.display_texture_stale,
        "queued for upload this frame"
    );
    // Newly-created runtimes are queued for texture upload.
    assert_eq!(
        layer.pending_upload_indices.len(),
        2,
        "both runtimes queued for upload"
    );

    // A second sync reconciles (no duplicates).
    layer.sync_from_doc(0, &doc);
    assert_eq!(
        layer.overlays.len(),
        2,
        "second sync does NOT duplicate runtimes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_interleave_doc_text_survives_empty_loader_completion() {
    // End-to-end interleave the unit test missed: a migrated chapter materializes text via
    // `sync_from_doc`, THEN the loader completes with an empty set. The doc text must SURVIVE.
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_interleave_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let img = ColorImage::filled([4, 3], Color32::GREEN);
    let file = persist::write_text_image(&dir, 0, "ta", &img).unwrap();
    let payload = persist::TextPayloadOut {
        uid: "ta".into(),
        name: "ta".into(),
        z: 1,
        layer_idx: 0,
        pinned: false,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        pinned_by_group: false,
        payload_uid: "ta".into(),
        render_data: json!({ "text": "ta" }),
        is_image: false,
        transform: crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 20.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        rendered_file: Some(file),
        mask_clip: None,
    };
    persist::write_page_text_payload(&dir, None, 0, &[payload]).unwrap();

    let mut doc = LayerDoc::new();
    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();

    let mut layer = TypingTextOverlayLayer::default();
    // 1) Early frame: doc materializes the text runtime (loader still in flight).
    layer.sync_from_doc(0, &doc);
    assert_eq!(layer.overlays.len(), 1, "doc-created the text runtime");

    // 2) Loader completes with an EMPTY decoded set (migrated chapter) — drive the exact merge step
    //    `poll_loader` runs. The doc-created runtime must NOT be wiped.
    let touched = merge_loaded_overlays(&mut layer.overlays, Vec::new());
    assert!(touched.is_empty());
    assert_eq!(
        layer.overlays.len(),
        1,
        "doc text SURVIVES the empty loader completion (race fixed)"
    );
    assert_eq!(layer.overlays[0].uid, "ta");

    let _ = std::fs::remove_dir_all(&dir);
}

fn decoded_text_overlay(uid: &str, page_idx: usize, center: [f32; 2]) -> TypingOverlayDecoded {
    TypingOverlayDecoded {
        uid: uid.into(),
        kind: TypingOverlayKind::Text,
        page_idx,
        center_page_px: center,
        mask_clip_enabled: false,
        layer_idx: 0,
        user_scale: 1.0,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name: crate::models::layer_model::persist::text_image_file_name(page_idx, uid),
        original_file_name: None,
        render_data_json: None,
        size_px: [2, 2],
        rgba: vec![0u8; 2 * 2 * 4],
        warnings: Vec::new(),
        extra: RenderedTextExtraInfo::default(),
    }
}

#[test]
fn loader_completion_merge_does_not_wipe_doc_created_runtimes() {
    // CRITICAL RACE (the intermittent "text shows then vanishes, sometimes half"): on a MIGRATED
    // chapter `sync_from_doc` materializes text runtimes from the doc on an early frame, then the
    // loader thread completes with an EMPTY decoded set (no `text_info.json`). The old wholesale
    // `self.overlays = decoded` WIPED the doc-created runtimes. The merge must leave them intact.
    let mut overlays: Vec<TypingOverlayRuntime> = vec![
        text_runtime_from_doc_node(
            "ta",
            0,
            [10.0, 20.0],
            1.0,
            0.0,
            None,
            false,
            false,
            1,
            None,
            [4, 3],
            vec![0u8; 4 * 3 * 4],
        ),
        text_runtime_from_doc_node(
            "tb",
            0,
            [30.0, 40.0],
            1.0,
            0.0,
            None,
            false,
            false,
            1,
            None,
            [4, 3],
            vec![0u8; 4 * 3 * 4],
        ),
    ];

    // Loader completes with an EMPTY set (migrated chapter).
    let touched = merge_loaded_overlays(&mut overlays, Vec::new());
    assert!(touched.is_empty(), "empty load touches nothing");
    assert_eq!(
        overlays.len(),
        2,
        "doc-created runtimes SURVIVE an empty loader completion"
    );
    assert!(overlays.iter().any(|o| o.uid == "ta"));
    assert!(overlays.iter().any(|o| o.uid == "tb"));
}

#[test]
fn loader_completion_merge_replaces_same_uid_without_duplicating() {
    // LEGACY/dup case: a doc-created runtime with uid "ta" exists (from the race), and the loader
    // returns the SAME uid "ta" (plus a brand-new "tc"). The merge must REPLACE "ta" in place (no
    // duplicate) and APPEND "tc".
    let mut overlays: Vec<TypingOverlayRuntime> = vec![text_runtime_from_doc_node(
        "ta",
        0,
        [10.0, 20.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        None,
        [4, 3],
        vec![0u8; 4 * 3 * 4],
    )];

    let touched = merge_loaded_overlays(
        &mut overlays,
        vec![
            decoded_text_overlay("ta", 0, [99.0, 88.0]),
            decoded_text_overlay("tc", 0, [1.0, 2.0]),
        ],
    );

    assert_eq!(
        overlays.len(),
        2,
        "same-uid REPLACED in place (no dup), new uid APPENDED"
    );
    let ta = overlays.iter().find(|o| o.uid == "ta").unwrap();
    assert_eq!(
        ta.center_page_px,
        [99.0, 88.0],
        "loaded entry replaced the doc-created one"
    );
    assert_eq!(
        overlays.iter().filter(|o| o.uid == "ta").count(),
        1,
        "no duplicate ta"
    );
    assert!(
        overlays.iter().any(|o| o.uid == "tc"),
        "new loaded overlay appended"
    );
    // Both the replaced and the appended entry are flagged for upload.
    assert_eq!(touched.len(), 2);
    // Same uid on a DIFFERENT page is NOT treated as a match (page-scoped key).
    let mut o2 = vec![text_runtime_from_doc_node(
        "ta",
        1,
        [5.0, 6.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        None,
        [4, 3],
        vec![0u8; 4 * 3 * 4],
    )];
    merge_loaded_overlays(&mut o2, vec![decoded_text_overlay("ta", 0, [7.0, 8.0])]);
    assert_eq!(
        o2.len(),
        2,
        "same uid on a different page is a distinct runtime"
    );
}

#[test]
fn image_overlay_render_data_round_trips_effects() {
    let effects = json!([{ "effect": "stroke", "width_px": 4 }]);
    let render_data = json!({ "effects": effects.clone() });
    let entry = build_storage_overlay_entry(
        "test-uid",
        TypingOverlayKind::Image,
        0,
        "image_p0_1_fx.png",
        Some("image_p0_1.png"),
        [10.0, 20.0],
        true,
        0,
        0.0,
        1.0,
        None,
        Some(render_data),
    );
    let obj = entry.as_object().expect("entry must be an object");
    assert_eq!(
        obj.get("image_original_file").and_then(Value::as_str),
        Some("image_p0_1.png")
    );
    let parsed = parse_image_overlay_render_data(obj);
    assert_eq!(
        effects_json_from_render_data(&parsed),
        serde_json::to_string(&effects).unwrap()
    );
    assert_eq!(
        parse_overlay_original_file_name(obj).as_deref(),
        Some("image_p0_1.png")
    );
}

#[test]
fn image_overlay_entry_omits_original_when_same_as_file() {
    // Когда исходник совпадает с показываемым файлом, дублирующий ключ не пишем.
    let entry = build_storage_overlay_entry(
        "test-uid",
        TypingOverlayKind::Image,
        0,
        "image_p0_1.png",
        Some("image_p0_1.png"),
        [0.0, 0.0],
        true,
        0,
        0.0,
        1.0,
        None,
        Some(default_render_data_for_image()),
    );
    let obj = entry.as_object().expect("entry must be an object");
    assert!(!obj.contains_key("image_original_file"));
}

fn shape_variant_test_params(text_shape: TextShape) -> TextRenderParams {
    TextRenderParams {
        text: "Просто без елок".to_string(),
        text_color: [0, 0, 0, 255],
        font_name: "font".to_string(),
        font_size_px: 24.0,
        line_spacing_px: 4.0,
        line_spacing_percent: 50.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: 120,
        align: HorizontalAlign::CENTER,
        selected_face_index: 0,
        force_bold: false,
        force_italic: false,
        faux_bold: None,
        faux_italic_slant_deg: None,
        uppercase_text: false,
        trim_extra_spaces: false,
        hanging_punctuation: false,
        new_line_after_sentence: false,
        enable_inline_style_tags: false,
        text_wrap_mode: TextWrapMode::Moderate,
        text_shape,
        shape_min_width_percent: 50.0,
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
        extra_info: crate::tabs::typing::render_next::types::RenderExtraInfoRequest::default(),
    }
}

#[test]
fn soft_peak_shape_menu_pairs_variants_with_wrap_strength() {
    let params = shape_variant_test_params(TextShape::SoftPeak);
    let variants = build_shape_variant_grid(&params);

    assert_eq!(variants.len(), 9);
    for (row, expected_variant) in [3, 9, 6].into_iter().enumerate() {
        let row_variants = variants
            .iter()
            .filter(|variant| variant.row == row)
            .collect::<Vec<_>>();
        assert_eq!(row_variants.len(), 3);
        assert!(
            row_variants
                .iter()
                .all(|variant| variant.width_px == params.width_px)
        );
        assert!(
            row_variants
                .iter()
                .all(|variant| variant.shape_min_width_percent == params.shape_min_width_percent)
        );
        assert!(
            row_variants
                .iter()
                .all(|variant| variant.shape_variant == expected_variant)
        );
        assert_eq!(row_variants[0].text_wrap_mode, TextWrapMode::Minimal);
        assert_eq!(row_variants[1].text_wrap_mode, TextWrapMode::Moderate);
        assert_eq!(row_variants[2].text_wrap_mode, TextWrapMode::Aggressive);
    }
}

#[test]
fn shape_variant_preview_does_not_depend_on_current_wrap_strength() {
    let mut params = shape_variant_test_params(TextShape::SoftPeak);
    params.text_wrap_mode = TextWrapMode::WholeWords;

    assert!(shape_variant_preview_available(TypingOverlayKind::Text));
    let variants = build_shape_variant_grid(&params);

    assert_eq!(variants.len(), 9);
    assert_eq!(variants[0].text_wrap_mode, TextWrapMode::Minimal);
    assert_eq!(variants[1].text_wrap_mode, TextWrapMode::Moderate);
    assert_eq!(variants[2].text_wrap_mode, TextWrapMode::Aggressive);
}

#[test]
fn canceled_shape_variant_preview_does_not_start_tiles() {
    let params = shape_variant_test_params(TextShape::SoftPeak);
    let variants = build_shape_variant_grid(&params);
    let cancel_render = Arc::new(AtomicBool::new(true));
    let fonts: Arc<dyn FontProvider> = Arc::new(FontContentSet::default());

    let tiles = render_shape_variant_preview_tiles(params, variants, &fonts, &cancel_render);

    assert!(tiles.is_empty());
}

#[test]
fn storage_normalization_preserves_soft_peak_shape() {
    let raw = json!({
        "schema_version": 2,
        "text_params": {
            "text": "Просто без елок",
            "font_path": "/tmp/font.ttf",
            "width_px": 120,
            "text_shape": "soft_peak",
            "shape_variant": 9
        },
        "effects": []
    });

    let Some(normalized) = normalize_render_data_value(&raw, 500) else {
        panic!("render data should normalize");
    };
    let Some(text_params) = normalized.get("text_params").and_then(Value::as_object) else {
        panic!("normalized render data should contain text params");
    };

    assert_eq!(
        text_params.get("text_shape").and_then(Value::as_str),
        Some("soft_peak")
    );
    assert_eq!(
        text_params.get("shape_variant").and_then(Value::as_u64),
        Some(9)
    );
}

#[test]
fn storage_normalization_preserves_formed_text_and_modern_fields() {
    let raw = json!({
        "schema_version": 2,
        "text_params": {
            "text": "Ты станешь выше и сильнее",
            "font_path": "/tmp/font.ttf",
            "width_px": 120,
            "formed_text": "Ты\nстанешь выше\nи сильнее",
            "kerning_px": 3.0,
            "hanging_punctuation": true,
            "new_line_after_sentence": true
        },
        "effects": []
    });

    let Some(normalized) = normalize_render_data_value(&raw, 500) else {
        panic!("render data should normalize");
    };
    let Some(text_params) = normalized.get("text_params").and_then(Value::as_object) else {
        panic!("normalized render data should contain text params");
    };

    assert_eq!(
        text_params.get("formed_text").and_then(Value::as_str),
        Some("Ты\nстанешь выше\nи сильнее"),
        "formed_text must survive normalization on project load"
    );
    // Устаревший `kerning_px` мигрирует в единый строковый ключ `kerning`.
    assert_eq!(
        text_params.get("kerning").and_then(Value::as_str),
        Some("3.00")
    );
    assert_eq!(
        text_params
            .get("hanging_punctuation")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        text_params
            .get("new_line_after_sentence")
            .and_then(Value::as_bool),
        Some(true)
    );
}

fn text_bubble(id: i64, u: f32, v: f32, translation: &str) -> Bubble {
    Bubble {
        id,
        img_idx: 0,
        img_u: u,
        img_v: v,
        side: None,
        bubble_class: None,
        bubble_type: None,
        text: translation.to_string(),
        original_text: String::new(),
        extra: serde_json::Map::new(),
    }
}

/// Builds an image bubble whose red rect spans the whole page and whose `text_areas` carry the
/// given anchors/translations. Area 0 mirrors its text into the legacy `text` field, matching
/// the persisted contract.
fn image_bubble_with_areas(id: i64, areas: &[((f32, f32), &str)]) -> Bubble {
    let mut extra = serde_json::Map::new();
    extra.insert("image_source_type".to_string(), Value::from("external"));
    // Red image-area rect spanning the whole page, in the persisted {p1,p2} object form.
    extra.insert(
        "rect_coords".to_string(),
        json!({
            "p1": {"img_u": 0.0, "img_v": 0.0},
            "p2": {"img_u": 1.0, "img_v": 1.0},
        }),
    );
    let items: Vec<Value> = areas
        .iter()
        .map(|((au, av), text)| {
            json!({
                "rect": [au - 0.02, av - 0.02, au + 0.02, av + 0.02],
                "anchor": [au, av],
                "original": "",
                "description": "",
                "translation": text,
            })
        })
        .collect();
    extra.insert("text_areas".to_string(), Value::Array(items));
    let primary = areas.first().map(|(_, text)| *text).unwrap_or_default();
    Bubble {
        id,
        img_idx: 0,
        img_u: areas.first().map(|((u, _), _)| *u).unwrap_or(0.5),
        img_v: areas.first().map(|((_, v), _)| *v).unwrap_or(0.5),
        side: None,
        bubble_class: Some("image".to_string()),
        bubble_type: None,
        text: primary.to_string(),
        original_text: String::new(),
        extra,
    }
}

#[test]
fn selection_seeds_text_from_each_image_area_anchor() {
    let page_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
    // One image bubble with three areas at distinct anchors.
    let bubbles = vec![image_bubble_with_areas(
        1,
        &[
            ((0.2, 0.2), "first"),
            ((0.5, 0.5), "second"),
            ((0.8, 0.8), "third"),
        ],
    )];

    // A small selection around the second area's anchor (50,50) must seed the second area's
    // text, not only area 0's. This is the regression: previously only `img_u/img_v` (area 0)
    // was considered, so later areas never matched a selection.
    let around =
        |u: f32, v: f32| Rect::from_center_size(scene_from_uv(page_rect, u, v), Vec2::splat(6.0));
    assert_eq!(
        pick_bubble_text_for_selection(&bubbles, 0, around(0.2, 0.2), page_rect),
        Some("first".to_string())
    );
    assert_eq!(
        pick_bubble_text_for_selection(&bubbles, 0, around(0.5, 0.5), page_rect),
        Some("second".to_string())
    );
    assert_eq!(
        pick_bubble_text_for_selection(&bubbles, 0, around(0.8, 0.8), page_rect),
        Some("third".to_string())
    );
}

#[test]
fn selection_picks_closest_anchor_and_skips_empty_text() {
    let page_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
    let bubbles = vec![
        text_bubble(1, 0.3, 0.3, "plain"),
        image_bubble_with_areas(2, &[((0.31, 0.31), ""), ((0.6, 0.6), "img-area")]),
    ];

    // Selection covers the plain bubble and the empty image area 0; the empty area is skipped
    // and the closest non-empty anchor (the plain bubble) wins.
    let selection = Rect::from_min_max(
        scene_from_uv(page_rect, 0.25, 0.25),
        scene_from_uv(page_rect, 0.35, 0.35),
    );
    assert_eq!(
        pick_bubble_text_for_selection(&bubbles, 0, selection, page_rect),
        Some("plain".to_string())
    );

    // A selection that contains no anchor returns None.
    let empty = Rect::from_min_max(
        scene_from_uv(page_rect, 0.9, 0.05),
        scene_from_uv(page_rect, 0.98, 0.12),
    );
    assert_eq!(
        pick_bubble_text_for_selection(&bubbles, 0, empty, page_rect),
        None
    );
}

// Legacy ribbon/page-index migration tests moved to `models::layer_model::text_payload` together
// with the `migrate_overlay_entries` logic (the single shared codec).

#[test]
fn decode_vector_mesh_warp_valid_object() {
    // 2x2 identity-ish mesh: 4 points, row-major.
    let value = serde_json::json!({
        "cols": 2,
        "rows": 2,
        "src_width_px": 512.0,
        "src_height_px": 200.0,
        "points_norm": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
    });
    let warp = decode_vector_mesh_warp(&value).expect("valid mesh should decode");
    assert_eq!(warp.cols, 2);
    assert_eq!(warp.rows, 2);
    assert!((warp.src_width_px - 512.0).abs() < f32::EPSILON);
    assert!((warp.src_height_px - 200.0).abs() < f32::EPSILON);
    assert_eq!(warp.points_norm.len(), 4);
    assert_eq!(warp.points_norm[3], [1.0, 1.0]);
}

#[test]
fn decode_vector_mesh_warp_wrong_points_len_is_none() {
    // cols*rows = 4 but only 3 points supplied.
    let value = serde_json::json!({
        "cols": 2,
        "rows": 2,
        "points_norm": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
    });
    assert!(decode_vector_mesh_warp(&value).is_none());
}

#[test]
fn decode_vector_mesh_warp_missing_keys_is_none() {
    // Missing `rows` and `points_norm`.
    let value = serde_json::json!({ "cols": 2 });
    assert!(decode_vector_mesh_warp(&value).is_none());
    // Degenerate grid (cols < 2) is rejected too.
    let degenerate = serde_json::json!({
        "cols": 1,
        "rows": 1,
        "points_norm": [[0.0, 0.0]],
    });
    assert!(decode_vector_mesh_warp(&degenerate).is_none());
}

#[test]
fn vector_transform_layout_gating_predicate() {
    // Enabled for Normal / Shape / CustomVectorLines; disabled for Formula / CustomRasterLines.
    for mode in [
        TextLayoutMode::Normal,
        TextLayoutMode::Shape,
        TextLayoutMode::CustomVectorLines,
    ] {
        assert!(
            vector_transform_allowed_for_layout_mode(mode),
            "{mode:?} must allow the vector transform"
        );
    }
    for mode in [TextLayoutMode::Formula, TextLayoutMode::CustomRasterLines] {
        assert!(
            !vector_transform_allowed_for_layout_mode(mode),
            "{mode:?} must NOT allow the vector transform"
        );
    }
}

#[test]
fn vector_base_reuses_current_texture_when_no_warp() {
    // Phase 3b shortcut: an overlay with NO stored `raster_transform` already IS the un-warped
    // base, so `request_vector_transform_base` reuses its resident `source_rgba` directly with NO
    // extra render (no in-flight render receiver).
    let mut layer = TypingTextOverlayLayer::default();
    let rgba = vec![7u8; 2 * 2 * 4];
    let overlay = text_runtime_from_doc_node(
        "t0",
        0,
        [10.0, 20.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        Some(json!({ "text_params": { "text": "hi" } })), // no raster_transform
        [2, 2],
        rgba.clone(),
    );
    layer.overlays.push(overlay);
    layer.transform_mode_overlay_idx = Some(0);
    layer.transform_mode_kind = TypingTransformModeKind::Vector;

    layer.request_vector_transform_base(0);

    let base = layer
        .vector_transform_base
        .as_ref()
        .expect("no-warp overlay yields an immediate reused base");
    assert_eq!(base.overlay_idx, 0);
    assert_eq!(base.size_px, [2, 2]);
    assert_eq!(
        base.rgba, rgba,
        "base reuses the un-warped source_rgba verbatim"
    );
    assert!(
        layer.vector_transform_base_rx.is_none(),
        "no background render is spawned for the no-warp reuse shortcut"
    );
}

#[test]
fn vector_base_render_is_skipped_when_token_superseded() {
    // The one-off un-warped base render early-outs (no render, no fonts, no disk) when its token is
    // no longer the latest — the cancellation contract that lets a re-enter / target change drop a
    // superseded render.
    let latest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(5));
    let request = TypingVectorBaseRenderRequest {
        token: 3, // stale: latest is 5
        latest_token: std::sync::Arc::clone(&latest),
        overlay_idx: 0,
        // A font NAME is the only required key now; it is never resolved because the stale token
        // short-circuits before any render.
        render_params: text_render_params_from_render_data(
            &json!({ "text_params": { "text": "x", "font_label": "SomeFont" } }),
        )
        .expect("params build when a font name is present"),
        font_provider: Arc::new(FontContentSet::default()),
    };
    let out = render_vector_transform_base(request).expect("stale token returns Ok, not Err");
    assert!(out.is_none(), "a superseded base render produces no result");
}

#[test]
fn vector_preview_active_requires_drag_and_base() {
    // The plain-PNG hide gate: the live warped preview draws only with VECTOR mode + an active drag
    // on the overlay + an available un-warped base.
    let mut layer = TypingTextOverlayLayer::default();
    layer.overlays.push(text_runtime_from_doc_node(
        "t0",
        0,
        [10.0, 20.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        None,
        [2, 2],
        vec![0u8; 2 * 2 * 4],
    ));
    layer.transform_mode_overlay_idx = Some(0);
    layer.transform_mode_kind = TypingTransformModeKind::Vector;
    layer.vector_transform_base = Some(TypingVectorTransformBaseTexture {
        overlay_idx: 0,
        size_px: [2, 2],
        rgba: vec![0u8; 2 * 2 * 4],
        texture: None,
    });
    // Base present but no drag yet -> not active (idle shows the baked PNG).
    assert!(!layer.vector_transform_preview_active(0));

    layer.vector_transform_drag = Some(TypingVectorTransformDragState {
        overlay_idx: 0,
        page_idx: 0,
        pointer_start_scene: Pos2::ZERO,
        mode: TypingOverlayDragMode::MoveMesh,
        start_mesh: default_deform_mesh_for_page([10.0, 20.0], [2, 2], 1.0, 0.0, [100, 100]),
        has_changes: false,
    });
    assert!(
        layer.vector_transform_preview_active(0),
        "drag + base -> preview active"
    );

    // Base for a DIFFERENT overlay does not activate the preview.
    layer.vector_transform_base.as_mut().unwrap().overlay_idx = 1;
    assert!(!layer.vector_transform_preview_active(0));
}

#[test]
fn overlay_text_layout_mode_reads_and_defaults() {
    let rd = serde_json::json!({ "text_params": { "text_layout_mode": "custom_vector_lines" } });
    assert_eq!(
        overlay_text_layout_mode(Some(&rd)),
        TextLayoutMode::CustomVectorLines
    );
    // Absent / no render_data -> Normal.
    assert_eq!(overlay_text_layout_mode(None), TextLayoutMode::Normal);
    let empty = serde_json::json!({ "text_params": {} });
    assert_eq!(
        overlay_text_layout_mode(Some(&empty)),
        TextLayoutMode::Normal
    );
}

#[test]
fn vector_footprint_round_trips_identity_and_warp() {
    // Page-px <-> normalized round-trip over an oriented, scaled footprint. For any (u, v) the
    // inverse of the footprint mapping returns (u, v) — the core invariant that makes an identity
    // working mesh settle to identity `points_norm` (a renderer no-op).
    let center = [400.0f32, 260.0];
    let (src_w, src_h) = (512.0f32, 200.0);
    let scale = 1.3f32;
    let angle = 27.0f32; // degrees

    for &(u, v) in &[
        (0.0f32, 0.0f32),
        (1.0, 0.0),
        (0.0, 1.0),
        (1.0, 1.0),
        (0.5, 0.5),
        (0.25, 0.8),
    ] {
        let page = vector_footprint_page_point(center, src_w, src_h, scale, angle, u, v);
        let back = vector_footprint_local_uv(center, src_w, src_h, scale, angle, page);
        assert!(
            (back[0] - u).abs() < 1e-3 && (back[1] - v).abs() < 1e-3,
            "identity round-trip failed for ({u},{v}): got {back:?}"
        );
    }

    // A KNOWN warp: displace one node's normalized position, map it to page px, and confirm the
    // inverse recovers the warped (u, v) — not the identity position.
    let warped_uv = [0.7f32, 0.35];
    let page = vector_footprint_page_point(
        center,
        src_w,
        src_h,
        scale,
        angle,
        warped_uv[0],
        warped_uv[1],
    );
    let back = vector_footprint_local_uv(center, src_w, src_h, scale, angle, page);
    assert!(
        (back[0] - warped_uv[0]).abs() < 1e-3 && (back[1] - warped_uv[1]).abs() < 1e-3,
        "known-warp round-trip failed: got {back:?}"
    );
    // The un-rotated center (u=v=0.5) must map exactly to the footprint center.
    let mid = vector_footprint_page_point(center, src_w, src_h, scale, angle, 0.5, 0.5);
    assert!((mid[0] - center[0]).abs() < 1e-3 && (mid[1] - center[1]).abs() < 1e-3);
}

#[test]
fn sample_points_norm_bilinear_identity_and_interpolation() {
    // Identity 3x3 grid samples back the identity coordinate.
    let mut identity = Vec::new();
    for i in 0..3 {
        for j in 0..3 {
            identity.push([j as f32 / 2.0, i as f32 / 2.0]);
        }
    }
    let s = sample_points_norm_bilinear(&identity, 3, 3, 0.25, 0.75);
    assert!((s[0] - 0.25).abs() < 1e-4 && (s[1] - 0.75).abs() < 1e-4);

    // Degenerate grid returns the query unchanged.
    let bad = sample_points_norm_bilinear(&[[0.0, 0.0]], 1, 1, 0.4, 0.6);
    assert!((bad[0] - 0.4).abs() < f32::EPSILON && (bad[1] - 0.6).abs() < f32::EPSILON);

    // Interpolate a translated grid (+0.1 x): every sample shifts by +0.1.
    let translated: Vec<[f32; 2]> = identity.iter().map(|p| [p[0] + 0.1, p[1]]).collect();
    let t = sample_points_norm_bilinear(&translated, 3, 3, 0.5, 0.5);
    assert!((t[0] - 0.6).abs() < 1e-4 && (t[1] - 0.5).abs() < 1e-4);
}

#[test]
fn text_render_params_round_trips_raster_transform() {
    let render_data = serde_json::json!({
        "text_params": {
            "text": "hi",
            "font_path": "/tmp/font.ttf",
            "raster_transform": {
                "cols": 3,
                "rows": 2,
                "src_width_px": 100.0,
                "src_height_px": 50.0,
                "points_norm": [
                    [0.0, 0.0], [0.5, 0.0], [1.0, 0.0],
                    [0.0, 1.0], [0.5, 1.0], [1.0, 1.0]
                ],
            },
        },
        "effects": [],
    });
    let params = text_render_params_from_render_data(&render_data).expect("params should parse");
    let warp = params.raster_transform.expect("warp should be present");
    assert_eq!(warp.cols, 3);
    assert_eq!(warp.rows, 2);
    assert_eq!(warp.points_norm.len(), 6);

    // Absent key -> None (identity / no warp).
    let no_warp = serde_json::json!({
        "text_params": { "text": "hi", "font_path": "/tmp/font.ttf" },
        "effects": [],
    });
    let params = text_render_params_from_render_data(&no_warp).expect("params should parse");
    assert!(params.raster_transform.is_none());
}

#[test]
fn normalize_preserves_raster_transform() {
    let obj = serde_json::json!({
        "text": "hi",
        "raster_transform": {
            "cols": 2,
            "rows": 2,
            "points_norm": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
        },
    });
    let normalized = normalize_text_params_object(obj.as_object().unwrap(), 512);
    let carried = normalized
        .get("raster_transform")
        .and_then(Value::as_object)
        .expect("raster_transform must survive normalize");
    assert_eq!(carried.get("cols").and_then(Value::as_u64), Some(2));
    assert_eq!(
        carried
            .get("points_norm")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(4)
    );
}

/// The font identity now reaches the renderer BY ORIGINAL FAMILY NAME. The codec's
/// name-resolution chain must prefer `font_original_name`, then fall back through
/// `font_label` -> `font_family` -> `font` -> the `font_path` file stem, so legacy
/// projects (which lack `font_original_name`) still open.
#[test]
fn render_params_font_name_prefers_original_name_then_falls_back() {
    let params = |tp: Value| {
        text_render_params_from_render_data(&json!({ "text_params": tp, "effects": [] }))
            .expect("params should parse")
            .font_name
    };
    // Original name wins over every other key.
    assert_eq!(
        params(json!({
            "text": "x",
            "font_original_name": "Anime Ace v05",
            "font_label": "основной",
            "font_family": "Ignored",
            "font": "Ignored2",
            "font_path": "/fonts/основной.ttf",
        })),
        "Anime Ace v05"
    );
    // No original name -> font_label (which on truly old data is a stem/label).
    assert_eq!(
        params(json!({ "text": "x", "font_label": "основной", "font_family": "Fam" })),
        "основной"
    );
    // Then font_family, then font, then the path stem.
    assert_eq!(params(json!({ "text": "x", "font_family": "Fam" })), "Fam");
    assert_eq!(params(json!({ "text": "x", "font": "Solo" })), "Solo");
    assert_eq!(
        params(json!({ "text": "x", "font_path": "/a/b/MyFont.ttf" })),
        "MyFont"
    );
    // Blank/whitespace-only names are skipped, not selected.
    assert_eq!(
        params(json!({ "text": "x", "font_original_name": "   ", "font_label": "Real" })),
        "Real"
    );
}

/// Bug-in-waiting fixed: the whitelist rebuilder must preserve `font_original_name`.
/// Dropping it would erase family-name resolution on every project re-normalize
/// (e.g. re-reading a legacy `text_info.json`).
#[test]
fn normalize_preserves_font_original_name() {
    // Present -> carried through verbatim.
    let obj = json!({ "text": "hi", "font_label": "стем", "font_original_name": "Real Family" });
    let normalized = normalize_text_params_object(obj.as_object().unwrap(), 512);
    assert_eq!(
        normalized.get("font_original_name").and_then(Value::as_str),
        Some("Real Family")
    );
    // Absent -> Null (the reader then falls back to font_label). Must not crash or fabricate.
    let legacy = json!({ "text": "hi", "font_label": "стем" });
    let normalized_legacy = normalize_text_params_object(legacy.as_object().unwrap(), 512);
    assert!(
        normalized_legacy
            .get("font_original_name")
            .is_some_and(Value::is_null),
        "absent original name normalizes to Null"
    );
}

/// Finding 10 (d): the codec leg keeps the seven faux keys through `normalize`
/// (with clamping) and applies the same `force_*` gate when building params.
#[test]
fn normalize_and_render_params_handle_faux_keys() {
    let obj = serde_json::json!({
        "text": "hi",
        "force_bold": true,
        "faux_bold": true,
        "faux_bold_thicken_percent": 99.0,
        "faux_bold_expand_percent": 4.0,
        "faux_bold_sharp_corners": false,
        "faux_bold_outward_only": false,
        "force_italic": true,
        "faux_italic": true,
        "faux_italic_slant_deg": -90.0,
    });
    let normalized = normalize_text_params_object(obj.as_object().unwrap(), 512);
    assert_eq!(normalized.get("faux_bold").and_then(Value::as_bool), Some(true));
    assert_eq!(
        normalized.get("faux_bold_thicken_percent").and_then(value_as_f32),
        Some(25.0) // 99 clamps to 25
    );
    assert_eq!(
        normalized.get("faux_bold_expand_percent").and_then(value_as_f32),
        Some(4.0)
    );
    assert_eq!(
        normalized.get("faux_bold_sharp_corners").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        normalized.get("faux_bold_outward_only").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(normalized.get("faux_italic").and_then(Value::as_bool), Some(true));
    assert_eq!(
        normalized.get("faux_italic_slant_deg").and_then(value_as_f32),
        Some(-45.0) // -90 clamps to -45
    );

    // force_* off -> faux gated to None even with faux_* on.
    let gated = serde_json::json!({
        "text_params": {
            "text": "hi",
            "font_path": "/tmp/font.ttf",
            "force_bold": false,
            "faux_bold": true,
            "faux_bold_thicken_percent": 7.5,
            "force_italic": false,
            "faux_italic": true,
            "faux_italic_slant_deg": -30.0,
        },
        "effects": [],
    });
    let params = text_render_params_from_render_data(&gated).expect("params should parse");
    assert!(params.faux_bold.is_none());
    assert!(params.faux_italic_slant_deg.is_none());

    // force_* on + faux_* on -> Some with the carried values.
    let enabled = serde_json::json!({
        "text_params": {
            "text": "hi",
            "font_path": "/tmp/font.ttf",
            "force_bold": true,
            "faux_bold": true,
            "faux_bold_thicken_percent": 7.5,
            "force_italic": true,
            "faux_italic": true,
            "faux_italic_slant_deg": -30.0,
        },
        "effects": [],
    });
    let params = text_render_params_from_render_data(&enabled).expect("params should parse");
    assert_eq!(params.faux_bold.map(|f| f.thicken_percent), Some(7.5));
    assert_eq!(params.faux_italic_slant_deg, Some(-30.0));
}

/// The canvas Shift+wheel font handler must defer ONLY when a real floating panel/popup
/// (above the Background canvas) is under the pointer. The Shift-drag selection-capture
/// overlay lives on `Order::Middle` with a known id and must NOT count as a panel, or
/// drag-selecting text over bare canvas would suppress the font handler.
#[test]
fn pointer_over_panel_over_canvas_defers_to_foreground_panel_only() {
    let ctx = egui::Context::default();
    let panel_rect = Rect::from_min_size(Pos2::new(500.0, 500.0), Vec2::new(120.0, 120.0));
    let capture_rect = Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(120.0, 120.0));

    // Two frames: the first registers the areas, the second exposes their rects to the
    // z-order hit-test that `layer_id_at` reads.
    for _ in 0..2 {
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let ctx = ui.ctx().clone();
            // A Foreground panel-like area (a real widget covering the whole rect).
            egui::Area::new(egui::Id::new("test_over_canvas_panel"))
                .order(egui::Order::Foreground)
                .fixed_pos(panel_rect.min)
                .show(&ctx, |ui| {
                    ui.allocate_rect(
                        Rect::from_min_size(ui.min_rect().min, panel_rect.size()),
                        egui::Sense::click(),
                    );
                });
            // The Shift-drag selection-capture overlay on its real (Middle) layer id.
            let capture = super::create_upload::shift_drag_capture_layer_id();
            egui::Area::new(capture.id)
                .order(capture.order)
                .fixed_pos(capture_rect.min)
                .show(&ctx, |ui| {
                    ui.allocate_rect(
                        Rect::from_min_size(ui.min_rect().min, capture_rect.size()),
                        egui::Sense::click(),
                    );
                });
        });
    }

    // Over the Foreground panel: the wheel belongs to that panel → defer.
    assert!(super::TypingTabState::pointer_over_panel_over_canvas(
        &ctx,
        panel_rect.center()
    ));
    // Over the Shift-capture overlay (Middle, matching id): bare canvas → do not defer.
    assert!(!super::TypingTabState::pointer_over_panel_over_canvas(
        &ctx,
        capture_rect.center()
    ));
    // Over empty space (no floating layer): bare canvas → do not defer.
    assert!(!super::TypingTabState::pointer_over_panel_over_canvas(
        &ctx,
        Pos2::new(5.0, 5.0)
    ));
}

#[test]
fn overlay_transform_rec_maps_runtime_placement_to_doc() {
    // Contract guard for the single source of truth used by BOTH the placement autosave and the
    // text edit-render doc route. The runtime stores rotation in DEGREES; the doc `TransformRec`
    // stores it in RADIANS. If this mapping drifts (e.g. someone drops `to_radians`), the top
    // rotation/scale sliders snap back after an edit re-render, because `sync_from_doc` reads the
    // doc transform back into the runtime. 90° must round-trip to exactly π/2 radians.
    let overlay = text_runtime_from_doc_node(
        "t0",
        0,
        [12.5, 34.0],
        2.0,
        90.0,
        None,
        false,
        false,
        0,
        None,
        [4, 3],
        vec![0u8; 4 * 3 * 4],
    );
    let transform = overlay.transform_rec();
    assert_eq!(transform.cx, 12.5, "cx passes through unchanged");
    assert_eq!(transform.cy, 34.0, "cy passes through unchanged");
    assert_eq!(transform.scale, 2.0, "scale passes through unchanged");
    assert!(
        (transform.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-6,
        "90 deg maps to π/2 rad, got {}",
        transform.rotation
    );
}

#[test]
fn all_page_indices_resident_transitions() {
    // The deterministic residency core of `all_pages_loaded`: a page counts as resident only when
    // it is BOTH projected here (`raster_layers_by_page` key) AND loaded in the shared doc.
    use crate::models::layer_model::layer_doc::{DecodedPagePayload, LayerDoc};
    use std::sync::{Arc, Mutex};

    let mut layer = TypingTextOverlayLayer::default();
    // No doc wired → nothing can be resident.
    assert!(!layer.all_page_indices_resident(&[0, 1]));

    let doc = Arc::new(Mutex::new(LayerDoc::new()));
    layer.set_layer_doc(Arc::clone(&doc));
    // Doc wired but no pages loaded → still false.
    assert!(!layer.all_page_indices_resident(&[0, 1]));

    // Make page 0 fully resident: loaded in the doc AND projected here.
    doc.lock().unwrap().insert_decoded_page(
        0,
        DecodedPagePayload {
            nodes: Vec::new(),
            groups: Vec::new(),
        },
    );
    layer.raster_layers_by_page.insert(0, Vec::new());
    assert!(layer.all_page_indices_resident(&[0]));
    assert!(
        !layer.all_page_indices_resident(&[0, 1]),
        "page 1 not resident yet"
    );

    // A page loaded in the doc but NOT projected here does not count (this tab has not synced it).
    doc.lock().unwrap().insert_decoded_page(
        1,
        DecodedPagePayload {
            nodes: Vec::new(),
            groups: Vec::new(),
        },
    );
    assert!(
        !layer.all_page_indices_resident(&[0, 1]),
        "doc-resident but not projected here → not loaded for this tab"
    );

    // Project page 1 too → both resident.
    layer.raster_layers_by_page.insert(1, Vec::new());
    assert!(layer.all_page_indices_resident(&[0, 1]));
}

#[test]
fn preload_apply_preserves_edits_and_deletions() {
    // The guarantee the preloader's apply path relies on: applying a freshly decoded (stale) page
    // through the MEMOIZED `insert_decoded_page` must NOT clobber a resident page's unsaved
    // in-memory edit and must NOT resurrect a node deleted this session. This is exactly the step
    // `drive_page_preload` performs for each decoded payload.
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::manifest::TransformRec;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_preload_apply_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Two text overlays on page 0, persisted to disk (uid "ta" at cx=10, "tb" at cx=30).
    let mut payloads = Vec::new();
    for (uid, cx) in [("ta", 10.0_f32), ("tb", 30.0_f32)] {
        let img = ColorImage::filled([4, 3], Color32::GREEN);
        let file = persist::write_text_image(&dir, 0, uid, &img).unwrap();
        payloads.push(persist::TextPayloadOut {
            uid: uid.into(),
            name: uid.into(),
            z: if uid == "ta" { 1 } else { 2 },
            layer_idx: 0,
            pinned: true,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            pinned_by_group: false,
            payload_uid: uid.into(),
            render_data: json!({ "text": uid }),
            is_image: false,
            transform: TransformRec {
                cx,
                cy: 20.0,
                rotation: 0.0,
                scale: 1.0,
            },
            deform: None,
            rendered_file: Some(file),
            mask_clip: None,
        });
    }
    persist::write_page_text_payload(&dir, None, 0, &payloads).unwrap();

    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();
    assert!(doc.node(0, "ta").is_some());
    assert!(doc.node(0, "tb").is_some());

    // Session edits held ONLY in memory: move "ta", delete "tb" (never flushed to disk).
    doc.set_transform(
        0,
        "ta",
        TransformRec {
            cx: 999.0,
            cy: 20.0,
            rotation: 0.0,
            scale: 1.0,
        },
    );
    assert!(doc.remove_node(0, "tb"));

    // A STALE decode from disk (still carries "ta"@10 and the deleted "tb"), like a preload payload
    // that finished decoding before the edits. `decode_page_payload` is the exact off-thread fn the
    // preload worker runs; `insert_decoded_page` is the exact memoized apply the driver runs.
    let stale = LayerDoc::decode_page_payload(0, &dir, None, None, &page_sizes).unwrap();
    doc.insert_decoded_page(0, stale);

    let ta = doc.node(0, "ta").expect("ta still resident");
    assert_eq!(
        ta.transform.cx, 999.0,
        "unsaved in-memory edit survives the stale preload apply (not clobbered back to disk cx=10)"
    );
    assert!(
        doc.node(0, "tb").is_none(),
        "session deletion is not resurrected by the stale preload apply"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn export_overlay_snapshot_is_empty_before_residency_and_populated_after() {
    // Phase 2 ORDERING FIX (the latent export bug): a migrated/v3 chapter's text overlays for a
    // never-visited page are materialized into `self.overlays` only when the page becomes resident
    // (`sync_from_doc`). Building the export overlay snapshot BEFORE that (the old bug) yields an
    // EMPTY snapshot for the page → text silently dropped from PNG/PSD. After the residency pass /
    // whole-project preload materializes the page, the SAME snapshot builder includes its text. This
    // is the deterministic core of the fix; the async export dispatch itself (thread + egui ctx) is
    // GUI-coupled and exercised only through the live drive point (documented in MODULE_README).
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::models::layer_model::persist;
    use std::collections::HashMap;

    let dir = std::env::temp_dir().join(format!("typ_export_order_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A migrated chapter: page 0 has an inline v3 text node with a real rendered PNG, no text_info.json.
    let img = ColorImage::filled([4, 3], Color32::GREEN);
    let file = persist::write_text_image(&dir, 0, "ta", &img).unwrap();
    let payload = persist::TextPayloadOut {
        uid: "ta".into(),
        name: "ta".into(),
        z: 1,
        layer_idx: 0,
        pinned: true,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        pinned_by_group: false,
        payload_uid: "ta".into(),
        render_data: json!({ "text": "ta" }),
        is_image: false,
        transform: crate::models::layer_model::manifest::TransformRec {
            cx: 10.0,
            cy: 20.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        rendered_file: Some(file),
        mask_clip: None,
    };
    persist::write_page_text_payload(&dir, None, 0, &[payload]).unwrap();

    let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes).unwrap();

    // Migrated-chapter state: the page is loaded in the doc but NOT yet projected into this tab, so
    // `self.overlays` is empty — exactly the pre-preload state of a never-visited page.
    let mut layer = TypingTextOverlayLayer::default();
    assert!(layer.overlays.is_empty());
    assert!(
        layer.build_export_overlay_snapshots().is_empty(),
        "BUG REPRO: snapshotting before residency drops the page's text (empty snapshot)"
    );

    // The residency pass (`ensure_raster_layers_for_page` -> `sync_from_doc`, or a preload apply)
    // materializes the doc's text node into `self.overlays`.
    layer.sync_from_doc(0, &doc);
    assert_eq!(
        layer.overlays.len(),
        1,
        "text node materialized after residency"
    );

    // FIX: the same snapshot builder, run AFTER residency, now includes the page's text.
    let snapshot = layer.build_export_overlay_snapshots();
    let page0 = snapshot
        .get(&0)
        .expect("page 0 present in the export snapshot after residency");
    assert_eq!(
        page0.len(),
        1,
        "the materialized text overlay is in the export snapshot"
    );
    assert_eq!(page0[0].uid, "ta");
    assert_eq!(page0[0].center_page_px, [10.0, 20.0]);
    assert_eq!(page0[0].size_px, [4, 3]);
    assert_eq!(page0[0].source_rgba.len(), 4 * 3 * 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn export_dispatch_gate_pass_completion_masks_and_mutual_exclusion() {
    // The pure export dispatch gate (`export_dispatch_ready`) is the testable core of
    // `run_pending_export_if_ready`. It gates on preload-pass COMPLETION (not residency), the mask
    // loader, and mutual exclusion with save.

    // FINDING 1: once the preload pass has drained, the export dispatches even though a page that
    // failed to decode is still NOT resident — no residency term appears in the gate, so it cannot
    // hang on the give-up path. Masks ready, no save busy.
    assert!(
        export_dispatch_ready(false, true, false),
        "pass drained + masks ready + no save → dispatch (no residency requirement → no hang)"
    );

    // Still waiting while the preload pass is applying pages.
    assert!(!export_dispatch_ready(true, true, false));

    // Masks not yet loaded → keep waiting (an empty mask snapshot would drop clip masks).
    assert!(!export_dispatch_ready(false, false, false));

    // FINDING 2: a project save is pending/in-flight → export must NOT dispatch, even when the
    // preload pass has drained and masks are ready (they share the preloader / doc / staging state).
    assert!(!export_dispatch_ready(false, true, true));
    // Every gate unmet at once stays blocked.
    assert!(!export_dispatch_ready(true, false, true));
}

#[test]
fn width_guide_drag_zero_offset_keeps_width() {
    // No pointer movement -> width unchanged.
    assert_eq!(width_from_guide_drag(300, true, 0.0, 1.0), 300);
    assert_eq!(width_from_guide_drag(300, false, 0.0, 2.0), 300);
}

#[test]
fn width_guide_drag_is_symmetric_about_center() {
    // The guide is centered, so a tick's source-px offset changes the TOTAL width by 2x. At scale 1.0,
    // a 20px rightward drag of the RIGHT tick widens by 40; the LEFT tick mirrors (a rightward drag
    // narrows by 40, a leftward drag widens by 40).
    assert_eq!(width_from_guide_drag(300, true, 20.0, 1.0), 340);
    assert_eq!(width_from_guide_drag(300, false, 20.0, 1.0), 260);
    assert_eq!(width_from_guide_drag(300, false, -20.0, 1.0), 340);
    assert_eq!(width_from_guide_drag(300, true, -20.0, 1.0), 260);
}

#[test]
fn width_guide_drag_divides_out_display_scale() {
    // A screen offset is converted to source px by dividing by the display scale (zoom * user_scale),
    // so the same on-screen drag yields a smaller source-px width change when zoomed/scaled up.
    // 40 screen px at scale 2.0 == 20 source px per side == 40 total. 300 + 40 = 340.
    assert_eq!(width_from_guide_drag(300, true, 40.0, 2.0), 340);
    // A non-positive scale must not divide by zero; it falls back to a tiny epsilon and stays finite.
    assert!(width_from_guide_drag(300, true, 0.0, 0.0) >= TEXT_OVERLAY_WIDTH_MIN_PX);
}

#[test]
fn width_guide_drag_clamps_to_settable_range() {
    // Dragging far past the bounds clamps to the min/max settable width.
    assert_eq!(
        width_from_guide_drag(300, false, 100_000.0, 1.0),
        TEXT_OVERLAY_WIDTH_MIN_PX
    );
    assert_eq!(
        width_from_guide_drag(300, true, 100_000.0, 1.0),
        TEXT_OVERLAY_WIDTH_MAX_PX
    );
}

// ---------------------------------------------------------------------------------------------
// Deferred text-layer save: the idle-debounce decision core.
//
// `placement_save_debounce_tick` is the whole state machine of the debounce, factored out of the
// frame loop so it can be tested without an `egui::Context`. The GUI-coupled half is deliberately
// not unit-tested (see the note at the end of this block).
// ---------------------------------------------------------------------------------------------

#[test]
fn debounce_does_not_flush_when_nothing_is_dirty() {
    // Not dirty => never a flush, and no window is kept open (so no repaint is scheduled).
    let (window, flush) = placement_save_debounce_tick(false, None, 100.0);
    assert_eq!(window, None);
    assert!(!flush);

    // Even if a window somehow lingered, clearing the dirty flag must retire it rather than fire.
    let (window, flush) = placement_save_debounce_tick(false, Some(1.0), 100.0);
    assert_eq!(window, None, "a stale window must not survive going clean");
    assert!(!flush);
}

#[test]
fn debounce_seeds_the_window_on_the_first_frame_after_a_mark() {
    // `mark_placement_save_dirty` leaves the window unseeded (`None`); the first frame to observe the
    // mark starts the clock at its own time and must NOT flush, however late that frame is.
    let (window, flush) = placement_save_debounce_tick(true, None, 500.0);
    assert_eq!(
        window,
        Some(500.0),
        "the first frame after a mark seeds the window at `now`"
    );
    assert!(
        !flush,
        "seeding must never flush: the debounce is measured from the seed, not from time zero"
    );
}

#[test]
fn debounce_holds_until_the_window_elapses_then_flushes() {
    let start = 10.0;

    // Inside the window: hold, and keep the SAME start so the deadline does not drift later each frame.
    let (window, flush) = placement_save_debounce_tick(true, Some(start), start + 0.1);
    assert_eq!(window, Some(start));
    assert!(!flush);

    let (window, flush) = placement_save_debounce_tick(
        true,
        Some(start),
        start + PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS - 0.001,
    );
    assert_eq!(window, Some(start));
    assert!(!flush, "must not flush one tick short of the debounce");

    // Exactly at the boundary the window has elapsed (the check is `>=`).
    let (window, flush) =
        placement_save_debounce_tick(true, Some(start), start + PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS);
    assert!(flush, "the window elapses at exactly the debounce duration");
    assert_eq!(window, None, "flushing retires the window");

    // Well past the deadline (e.g. the app was busy and skipped frames) still flushes exactly once.
    let (window, flush) = placement_save_debounce_tick(true, Some(start), start + 60.0);
    assert!(flush);
    assert_eq!(window, None);
}

#[test]
fn re_marking_restarts_the_debounce_window_so_a_drag_writes_nothing() {
    // This is the property that makes a continuous gesture cheap: every frame of a drag re-marks, and
    // a re-mark clears the seed (`mark_placement_save_dirty` sets `since = None`), so the next frame
    // re-seeds at ITS time and the window can never accumulate.
    let mut window: Option<f64> = None;

    // Simulate a 3-second drag at 10 fps: mark, tick, mark, tick, ...
    let mut now = 0.0_f64;
    for _ in 0..30 {
        window = None; // the frame's edit re-marks -> `mark_placement_save_dirty` clears the seed
        let (next_window, flush) = placement_save_debounce_tick(true, window, now);
        assert!(
            !flush,
            "a continuous drag must never reach the debounce, however long it runs (t={now})"
        );
        window = next_window;
        now += 0.1;
    }

    // The gesture settles: no more marks, so the window now survives from frame to frame and elapses.
    let settle_start = now;
    let (window_after_settle, flush) = placement_save_debounce_tick(true, window, now);
    assert!(!flush, "the frame right after the last mark must not flush");
    assert_eq!(
        window_after_settle,
        Some(settle_start - 0.1),
        "the window dates from the last marking frame's seed, not from the drag start"
    );

    let (_, flush) = placement_save_debounce_tick(
        true,
        window_after_settle,
        settle_start + PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS,
    );
    assert!(flush, "once the marks stop, the debounce fires");
}

#[test]
fn debounce_fires_one_window_after_the_last_mark_not_two() {
    // Ordering contract of the CALL SITE, expressed against the pure core: `drive_placement_save_debounce`
    // runs AFTER `canvas.draw`, so a mark made during frame L is observed by frame L's own tick and seeds
    // the window at L. Driving the tick BEFORE the marks (as it was first written) made the L-frame tick
    // see the PREVIOUS state: the frame after the last mark only re-seeded, pushing the flush to
    // ~L + 2 * PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS across two wakeups instead of one window.
    let last_mark_at = 4.0_f64;

    // Frame L: the edit marks (seed cleared), then this frame's tick observes it and seeds at L.
    let (window, flush) = placement_save_debounce_tick(true, None, last_mark_at);
    assert!(!flush);
    assert_eq!(window, Some(last_mark_at), "the marking frame seeds its own window");

    // The wake armed for exactly one window lands at L + 1.5 and flushes — no re-seed, no second wait.
    let (window, flush) = placement_save_debounce_tick(
        true,
        window,
        last_mark_at + PLACEMENT_SAVE_IDLE_DEBOUNCE_SECS,
    );
    assert!(
        flush,
        "the write lands one debounce window after the last edit, not two"
    );
    assert_eq!(window, None);
}

// ---------------------------------------------------------------------------------------------
// Deferred text-layer save: dirty state may retire ONLY when a write is genuinely dispatched.
// ---------------------------------------------------------------------------------------------

/// A layer whose text persistence IS wired (staging dir + shared doc), so dispatch decisions are not
/// short-circuited by `NotWired`. The dir need not exist: these tests never let a write reach disk.
fn wired_overlay_layer() -> TypingTextOverlayLayer {
    TypingTextOverlayLayer {
        layers_primary_dir: Some(std::env::temp_dir().join("typing_deferred_save_tests")),
        layer_doc: Some(Arc::new(Mutex::new(
            crate::models::layer_model::layer_doc::LayerDoc::new(),
        ))),
        ..Default::default()
    }
}

#[test]
fn unwired_flush_keeps_the_edit_dirty_instead_of_dropping_it() {
    // B1: `request_overlay_placement_save` silently writes NOTHING when no staging dir / doc is wired.
    // Clearing the dirty state before/regardless of that made the edit clean FOREVER: the debounce stops
    // arming repaints, `has_pending_placement_save` goes false, and tab-leave/exit never retry — at exit
    // that is silent data loss, because the barrier cannot cover a job that was never enqueued.
    let mut layer = TypingTextOverlayLayer::default(); // nothing wired
    layer.mark_placement_save_dirty();

    assert_eq!(
        layer.request_overlay_placement_save(),
        PlacementSaveDispatch::NotWired,
        "no dir/doc ⇒ the dispatch reports that nothing was written"
    );

    layer.flush_placement_save_if_dirty(TypingSaveFlushReason::SelectionChange);
    assert!(
        layer.has_pending_placement_save(),
        "a flush that dispatched nothing must leave the edit dirty for the next flush point"
    );
    assert!(layer.placement_save_dirty, "the placement axis specifically survives");
}

#[test]
fn unwired_inline_flush_keeps_the_edit_dirty_and_reports_why() {
    // Same contract on the INLINE (tab-leave / exit) path: `flush_text_layers` used to answer an empty
    // page set both when it could not run and when it ran with nothing resident, so the caller cleared
    // the dirty state either way. It now distinguishes them with a typed error.
    let mut layer = TypingTextOverlayLayer::default(); // nothing wired
    layer.mark_placement_save_dirty();

    assert!(
        matches!(
            layer.flush_text_layers(),
            Err(TypingTextFlushError::NoLayersDir)
        ),
        "a flush that could not run is an Err, not an empty owned set"
    );

    layer.flush_text_layers_if_dirty(TypingSaveFlushReason::Exit);
    assert!(
        layer.has_pending_placement_save(),
        "an exit flush that could not run must not mark the edit written"
    );

    // Wiring only the dir still leaves no store for the text: also an error, still dirty.
    layer.layers_primary_dir = Some(std::env::temp_dir().join("typing_deferred_save_tests"));
    assert!(matches!(
        layer.flush_text_layers(),
        Err(TypingTextFlushError::NoLayerDoc)
    ));
    assert!(layer.has_pending_placement_save());
}

#[test]
fn flush_that_ran_with_no_resident_pages_is_ok_and_settles_the_edit() {
    // The other side of the disambiguation: a wired flush over a doc with NO resident pages RAN — it
    // persisted the (empty) live state — so it returns Ok and legitimately retires the dirty flag. This
    // is the case an empty `HashSet` used to be conflated with.
    let mut layer = wired_overlay_layer();
    layer.mark_placement_save_dirty();

    let outcome = layer
        .flush_text_layers()
        .expect("a wired flush over an empty doc RUNS; it simply owns no pages");
    assert!(outcome.owned_pages.is_empty());
    assert_eq!(outcome.failed_pages, 0);
    assert!(
        !layer.has_pending_placement_save(),
        "a flush that ran and fully succeeded settles the deferred edit"
    );
}

#[test]
fn busy_parked_save_still_counts_as_pending() {
    // B4: `request_overlay_placement_save` PARKS (writes nothing) while a save/create/edit render is in
    // flight; `poll_save_jobs` / `poll_edit_overlay_jobs` re-fire it later. Until then the edit is as
    // unwritten as a dirty flag, so `has_pending_placement_save` must say so — otherwise an exit or
    // tab-leave flush sees "nothing pending", no-ops, and the parked write is dropped on close.
    let mut layer = wired_overlay_layer();
    // Stand in for a placement save already in flight. The sender is irrelevant: nothing polls `rx`.
    let (_unused_sender, rx) = mpsc::channel::<Result<(), String>>();
    layer.save_rx = Some(rx);
    layer.mark_placement_save_dirty();

    assert_eq!(
        layer.request_overlay_placement_save(),
        PlacementSaveDispatch::Parked,
        "a wired request behind an in-flight job parks rather than spawning a second writer"
    );

    let mut layer = wired_overlay_layer();
    let (_unused_sender, rx) = mpsc::channel::<Result<(), String>>();
    layer.save_rx = Some(rx);
    layer.mark_placement_save_dirty();
    layer.flush_placement_save_if_dirty(TypingSaveFlushReason::Idle);

    assert!(
        layer.save_requested_while_busy,
        "the flush handed the write to the parked re-fire"
    );
    assert!(
        !layer.placement_save_dirty,
        "parking IS a successful dispatch: the pipeline owns the write, so the axis retires"
    );
    assert!(
        layer.has_pending_placement_save(),
        "but the write has not happened yet, so exit/tab-leave must still see it as pending"
    );
}

#[test]
fn discarding_drops_the_parked_re_fire_too() {
    // B2: on the DISCARD path the staging dir is deleted and the saver is shut down. A surviving parked
    // re-fire would be dispatched afterwards and take `enqueue_page_text_save`'s SYNC fallback,
    // re-creating the deleted dir with the discarded edits — so discard must drop it, unlike the normal
    // `clear_placement_save_dirty` (where the flag is a write to protect, not one to cancel).
    let mut layer = wired_overlay_layer();
    layer.mark_placement_save_dirty();
    layer.save_requested_while_busy = true;

    layer.clear_placement_save_dirty();
    assert!(
        layer.save_requested_while_busy,
        "a normal clear must never cancel a parked re-fire (it may be an eager structural save)"
    );
    assert!(layer.has_pending_placement_save());

    layer.discard_pending_placement_save();
    assert!(!layer.save_requested_while_busy, "discard cancels the re-fire");
    assert!(
        !layer.has_pending_placement_save(),
        "after a discard nothing is pending: the exit dialog must not re-latch and reopen"
    );
}

#[test]
fn deferred_raster_geometry_enqueues_and_barrier_persists_transform_and_deform() {
    use crate::models::layer_model::{
        layer_doc::LayerDoc,
        manifest::{DeformRec, TransformRec},
        persist,
    };

    // This exercises the same deferred helpers used by keyboard scale/nudge and the image-panel
    // slider. `page_has_pending_save` proves the caller enqueued a saver job rather than performing
    // the old targeted manifest RMW; the FIFO barrier is the durability boundary before reload.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "typing_deferred_raster_geometry_{}_{}",
        std::process::id(),
        unique
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let image = ColorImage::filled([2, 2], Color32::WHITE);
    persist::add_page_raster(
        &dir,
        None,
        0,
        "raster",
        "Raster",
        true,
        1.0,
        TransformRec {
            cx: 1.0,
            cy: 2.0,
            rotation: 0.0,
            scale: 1.0,
        },
        &image,
    )
    .unwrap();

    let mut page_sizes = HashMap::new();
    page_sizes.insert(0, [100, 100]);
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &page_sizes)
        .unwrap_or_else(|err| panic!("could not load deferred-raster test layer: {err}"));
    doc.enable_background_saver();
    let doc = Arc::new(Mutex::new(doc));
    let mut layer = TypingTextOverlayLayer {
        layers_primary_dir: Some(dir.clone()),
        layer_doc: Some(Arc::clone(&doc)),
        ..Default::default()
    };

    let transform = TransformRec {
        cx: 30.0,
        cy: 40.0,
        rotation: 0.5,
        scale: 1.5,
    };
    layer.persist_raster_transform_deferred(0, "raster", transform);
    let handle = {
        let guard = doc
            .lock()
            .unwrap_or_else(|_| panic!("deferred-raster test document lock poisoned"));
        assert!(
            guard.page_has_pending_save(0),
            "the deferred transform must enqueue a page save before a durability barrier"
        );
        guard
            .saver_handle()
            .unwrap_or_else(|| panic!("deferred-raster test saver was not enabled"))
    };
    let failed_text_pages = handle.barrier_blocking();
    assert!(failed_text_pages.is_empty(), "deferred transform save failed");

    let after_transform = persist::load_page_rasters(&dir, None, 0)
        .unwrap_or_else(|err| panic!("could not reload deferred transform: {err}"));
    let persisted_transform = after_transform.layers[0].transform;
    assert_eq!(persisted_transform.cx, transform.cx);
    assert_eq!(persisted_transform.cy, transform.cy);
    assert_eq!(persisted_transform.rotation, transform.rotation);
    assert_eq!(persisted_transform.scale, transform.scale);

    let deform = DeformRec {
        cols: 2,
        rows: 2,
        points_px: vec![[10.0, 20.0], [30.0, 20.0], [10.0, 40.0], [30.0, 40.0]],
    };
    layer.persist_raster_deform_deferred(0, "raster", transform, Some(deform.clone()));
    let handle = doc
        .lock()
        .unwrap_or_else(|_| panic!("deferred-raster test document lock poisoned"))
        .saver_handle()
        .unwrap_or_else(|| panic!("deferred-raster test saver was not enabled"));
    let failed_text_pages = handle.barrier_blocking();
    assert!(failed_text_pages.is_empty(), "deferred deform save failed");

    let after_deform = persist::load_page_rasters(&dir, None, 0)
        .unwrap_or_else(|err| panic!("could not reload deferred deform: {err}"));
    let persisted_transform = after_deform.layers[0].transform;
    assert_eq!(persisted_transform.cx, transform.cx);
    assert_eq!(persisted_transform.cy, transform.cy);
    assert_eq!(persisted_transform.rotation, transform.rotation);
    assert_eq!(persisted_transform.scale, transform.scale);
    let Some(persisted_deform) = after_deform.layers[0].deform.as_ref() else {
        panic!("the barrier must persist the deferred deform mesh");
    };
    assert_eq!(persisted_deform.cols, deform.cols);
    assert_eq!(persisted_deform.rows, deform.rows);
    assert_eq!(persisted_deform.points_px, deform.points_px);

    doc.lock()
        .unwrap_or_else(|_| panic!("deferred-raster test document lock poisoned"))
        .shutdown_saver();
    if let Err(err) = std::fs::remove_dir_all(&dir) {
        panic!("could not remove deferred-raster test directory: {err}");
    }
}

// NOT unit-tested here, and why: the surrounding `drive_placement_save_debounce` needs a live
// `egui::Context` (it reads `input(|i| i.time)` and calls `request_repaint_after`), and the flush
// points that write for real (`flush_placement_save_on_page_change`,
// `flush_edit_save_on_selection_change`) need staging dirs and a running saver thread — i.e. an
// integration harness, not a unit test. The decision they all delegate to is the pure fn above, and
// the dispatch/dirty contract they share is covered by the tests above; `flush_text_layers`'s own
// per-page persistence contract is covered by the `layer_model::persist` tests.
//
// Also NOT unit-tested (needs a running GUI): that `MangaApp::start_exit_cleanup` → `on_exit` performs
// no write, and that the exit dialog does not reopen after "Не сохранять". Both live in `MangaApp`,
// which cannot be constructed without a project, an eframe context, and the loader threads.

// ===================== Centering assist geometry =====================

/// Asserts two page-px points are within a small tolerance.
fn assert_page_close(a: [f32; 2], b: [f32; 2], what: &str) {
    let eps = 1e-3;
    assert!(
        (a[0] - b[0]).abs() <= eps && (a[1] - b[1]).abs() <= eps,
        "{what}: {a:?} vs {b:?}"
    );
}

/// Looks up one corner's page-px position from a frame at a given total visual angle.
fn frame_corner_pos(
    frame: &CenteringFrame,
    angle_deg: f32,
    corner: CenteringFrameCorner,
) -> [f32; 2] {
    let all = centering_frame_corners_page_px(frame, angle_deg);
    let idx = CenteringFrameCorner::ALL
        .iter()
        .position(|c| *c == corner)
        .expect("corner is one of ALL");
    all[idx]
}

#[test]
fn centering_affine_chosen_center_maps_offset_scale_and_rotation() {
    let center = [100.0, 100.0];
    let size = [40, 20];
    // No rotation, unit scale: a +10px x offset from the image center lands +10px in page x.
    assert_page_close(
        affine_chosen_center_page_px(center, size, 1.0, 0.0, [30.0, 10.0]),
        [110.0, 100.0],
        "affine no-rotation offset",
    );
    // Scale doubles the local offset before placing it.
    assert_page_close(
        affine_chosen_center_page_px(center, size, 2.0, 0.0, [30.0, 10.0]),
        [120.0, 100.0],
        "affine scaled offset",
    );
    // 90deg (screen y-down) rotates the +x local offset into +y.
    assert_page_close(
        affine_chosen_center_page_px(center, size, 1.0, 90.0, [30.0, 10.0]),
        [100.0, 110.0],
        "affine rotated offset",
    );
    // The exact image center always maps to the placement center regardless of angle/scale.
    assert_page_close(
        affine_chosen_center_page_px(center, size, 1.7, 37.0, [20.0, 10.0]),
        center,
        "image center is placement center",
    );
}

#[test]
fn centering_chosen_center_falls_back_to_image_center_when_extras_none() {
    // A doc-materialized overlay carries all-`None` extras; Mean/Median must fall back to the plain
    // image center, so the chosen center equals the placement center (reconciliation delta zero).
    let overlay = text_runtime_from_doc_node(
        "c0",
        0,
        [50.0, 60.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        None,
        [40, 20],
        vec![0u8; 40 * 20 * 4],
    );
    let page_size = [200, 200];
    for kind in [
        CenteringAssistCenterKind::Image,
        CenteringAssistCenterKind::Mean,
        CenteringAssistCenterKind::Median,
    ] {
        assert_page_close(
            centering_chosen_center_page_px(&overlay, kind, page_size),
            [50.0, 60.0],
            "fallback to image center",
        );
    }
}

#[test]
fn centering_chosen_center_uses_mean_when_present() {
    let mut overlay = text_runtime_from_doc_node(
        "c1",
        0,
        [50.0, 60.0],
        1.0,
        0.0,
        None,
        false,
        false,
        0,
        None,
        [40, 20],
        vec![0u8; 40 * 20 * 4],
    );
    // Mean center 10px left / 10px up of the image center [20,10] -> page offset (-10, 0).
    overlay.extra.mean_center = Some([10.0, 10.0]);
    let page_size = [200, 200];
    assert_page_close(
        centering_chosen_center_page_px(&overlay, CenteringAssistCenterKind::Mean, page_size),
        [40.0, 60.0],
        "mean center used",
    );
    // Median still absent -> falls back to the image center.
    assert_page_close(
        centering_chosen_center_page_px(&overlay, CenteringAssistCenterKind::Median, page_size),
        [50.0, 60.0],
        "median falls back",
    );
}

#[test]
fn centering_frame_corner_drag_lands_pointer_and_fixes_opposite() {
    let frame = CenteringFrame {
        center_page_px: [100.0, 100.0],
        half_size_page_px: [20.0, 10.0],
    };
    let angle = 0.0;
    let start_tl = frame_corner_pos(&frame, angle, CenteringFrameCorner::TopLeft);
    // Drag the bottom-right corner far out to [140, 130].
    let (center, half) = centering_frame_corner_drag(
        frame.center_page_px,
        frame.half_size_page_px,
        CenteringFrameCorner::BottomRight,
        [140.0, 130.0],
        angle,
        4.0,
    );
    let result = CenteringFrame {
        center_page_px: center,
        half_size_page_px: half,
    };
    assert_page_close(
        frame_corner_pos(&result, angle, CenteringFrameCorner::BottomRight),
        [140.0, 130.0],
        "dragged corner lands under pointer",
    );
    assert_page_close(
        frame_corner_pos(&result, angle, CenteringFrameCorner::TopLeft),
        start_tl,
        "opposite corner stays fixed",
    );
}

#[test]
fn centering_frame_corner_drag_fixes_opposite_when_rotated() {
    let frame = CenteringFrame {
        center_page_px: [100.0, 100.0],
        half_size_page_px: [20.0, 10.0],
    };
    let angle = 30.0;
    let start_tr = frame_corner_pos(&frame, angle, CenteringFrameCorner::TopRight);
    // Pick a pointer far from the fixed (top-right) corner and drag the bottom-left corner there.
    let (center, half) = centering_frame_corner_drag(
        frame.center_page_px,
        frame.half_size_page_px,
        CenteringFrameCorner::BottomLeft,
        [40.0, 150.0],
        angle,
        4.0,
    );
    let result = CenteringFrame {
        center_page_px: center,
        half_size_page_px: half,
    };
    assert_page_close(
        frame_corner_pos(&result, angle, CenteringFrameCorner::BottomLeft),
        [40.0, 150.0],
        "rotated dragged corner lands under pointer",
    );
    assert_page_close(
        frame_corner_pos(&result, angle, CenteringFrameCorner::TopRight),
        start_tr,
        "rotated opposite corner stays fixed",
    );
}

#[test]
fn centering_frame_corner_drag_clamps_min_and_keeps_opposite_fixed() {
    let frame = CenteringFrame {
        center_page_px: [100.0, 100.0],
        half_size_page_px: [20.0, 10.0],
    };
    let angle = 0.0;
    let start_tl = frame_corner_pos(&frame, angle, CenteringFrameCorner::TopLeft);
    // Collapse the frame by dragging BR onto TL: half-size clamps to the minimum, opposite still fixed.
    let (center, half) = centering_frame_corner_drag(
        frame.center_page_px,
        frame.half_size_page_px,
        CenteringFrameCorner::BottomRight,
        start_tl,
        angle,
        4.0,
    );
    assert!(half[0] >= 4.0 - 1e-4 && half[1] >= 4.0 - 1e-4, "half clamped to min: {half:?}");
    let result = CenteringFrame {
        center_page_px: center,
        half_size_page_px: half,
    };
    assert_page_close(
        frame_corner_pos(&result, angle, CenteringFrameCorner::TopLeft),
        start_tl,
        "opposite corner fixed even when collapsed",
    );
}

#[test]
fn centering_reconcile_converges_in_one_move_for_unreachable_frame() {
    // An affine overlay whose chosen center sits 10px right of its placement center. The frame center
    // is far off the right page edge (unreachable). The reconcile target must land the overlay at the
    // visibility boundary in ONE move, and a SECOND invocation from that fed-back state must not move
    // it again — i.e. the constrained target is a fixed point.
    let page_size = [200usize, 200usize];
    let half_extent = [20.0f32, 10.0f32]; // affine box half-size (page px)
    let chosen_offset = [10.0f32, 0.0f32]; // chosen center relative to the placement center
    let frame_center = [500.0f32, 100.0f32]; // off-page, unreachable

    // Given a placement center, derive the (chosen, page-px bounds) an affine overlay would present.
    let derive = |center: [f32; 2]| -> ([f32; 2], egui::Rect) {
        let chosen = [center[0] + chosen_offset[0], center[1] + chosen_offset[1]];
        let bounds = egui::Rect::from_min_max(
            egui::Pos2::new(center[0] - half_extent[0], center[1] - half_extent[1]),
            egui::Pos2::new(center[0] + half_extent[0], center[1] + half_extent[1]),
        );
        (chosen, bounds)
    };

    let start = [100.0f32, 100.0f32];
    let (chosen0, bounds0) = derive(start);
    let target1 = centering_reconcile_target_center(
        start, chosen0, frame_center, bounds0, false, false, page_size,
    );
    // It moved (the frame pulls it toward the page edge) and stopped at the visibility boundary: with
    // TEXT_OVERLAY_MIN_VISIBLE_FRACTION = 0.10 and a 40px-wide box, 4px must stay inside the 200px page,
    // so the box's right edge lands on x = 200 + 4 - 40 ... i.e. the box left edge sits at 196 and the
    // center at 216.
    assert!(
        (target1[0] - start[0]).abs() > CENTERING_RECONCILE_EPS_PX,
        "first reconcile must move the overlay: {target1:?}"
    );
    assert_page_close(target1, [216.0, 100.0], "converged to visibility boundary");

    // Feed the result back (the box and chosen center translate rigidly with the placement center) and
    // reconcile again with identical logical inputs: the target must be the SAME point (no movement).
    let (chosen1, bounds1) = derive(target1);
    let target2 = centering_reconcile_target_center(
        target1, chosen1, frame_center, bounds1, false, false, page_size,
    );
    assert_page_close(target2, target1, "second reconcile is a no-op (fixed point)");
    let dx = target2[0] - target1[0];
    let dy = target2[1] - target1[1];
    assert!(
        dx.hypot(dy) <= CENTERING_RECONCILE_EPS_PX,
        "no further movement within epsilon: {target2:?} vs {target1:?}"
    );
}

#[test]
fn translate_rigid_preserves_mesh_shape_and_clamps_box() {
    // A rigid translation toward an unreachable target must shift EVERY control point by the same
    // allowed delta (internal shape preserved) and stop when the control-point box reaches the page's
    // overlay bound — never squashing points independently.
    let page_size = [100usize, 100usize];
    let points = vec![[10.0f32, 10.0], [30.0, 10.0], [10.0, 40.0], [30.0, 40.0]];
    let before = points.clone();
    let mut mesh = TypingOverlayDeformMesh::new(2, 2, points, page_size)
        .expect("2x2 mesh with 4 points is valid");

    // Ask for a far-too-large +x shift; the box (x in [10, 30]) can move until its right edge hits the
    // overlay bound overlay_uv_max * 100 = 190, i.e. an allowed dx of 160. y stays put.
    let applied = mesh.translate_rigid(1000.0, 0.0, page_size);
    assert!(
        (applied[0] - 160.0).abs() <= 1e-3 && applied[1].abs() <= 1e-3,
        "allowed delta clamps the box to the page bound: {applied:?}"
    );

    // Relative offsets between every point and the first are unchanged: the mesh did not deform.
    for (idx, (after, orig)) in mesh.points_px.iter().zip(before.iter()).enumerate() {
        let expected = [orig[0] + applied[0], orig[1] + applied[1]];
        assert!(
            (after[0] - expected[0]).abs() <= 1e-3 && (after[1] - expected[1]).abs() <= 1e-3,
            "point {idx} shifted rigidly: {after:?} vs {expected:?}"
        );
    }
    // The whole box stayed within the page's overlay bounds (right edge exactly on the bound).
    let bounds = deform_mesh_bounds_px(&mesh);
    assert!(bounds.right() <= 190.0 + 1e-3, "box right within bound: {}", bounds.right());
}

#[test]
fn push_rotation_handle_clear_moves_past_obstacles_on_ray() {
    let corner = Pos2::new(100.0, 100.0);
    let handle = Pos2::new(124.0, 100.0); // outward dir = +x
    let clearance = 20.0;

    // No obstacles / far obstacles: untouched.
    assert_eq!(push_rotation_handle_clear(corner, handle, &[], clearance), handle);
    let far = [Pos2::new(0.0, 0.0)];
    assert_eq!(push_rotation_handle_clear(corner, handle, &far, clearance), handle);

    // An obstacle sitting exactly on the handle pushes it forward until `clearance` away.
    let on_top = [handle];
    let cleared = push_rotation_handle_clear(corner, handle, &on_top, clearance);
    assert!(
        (cleared.distance(handle) - clearance).abs() <= 1e-3 && cleared.x > handle.x,
        "pushed forward to exactly the clearance: {cleared:?}"
    );

    // Two obstacles ahead ON the ray with overlapping clearance zones: lands past BOTH.
    let chain = [Pos2::new(130.0, 100.0), Pos2::new(160.0, 100.0)];
    let cleared = push_rotation_handle_clear(corner, handle, &chain, clearance);
    for obstacle in &chain {
        assert!(
            cleared.distance(*obstacle) >= clearance - 1e-3,
            "clear of every obstacle: {cleared:?} vs {obstacle:?}"
        );
    }
    assert!((cleared.y - 100.0).abs() <= 1e-3, "stays on the outward ray: {cleared:?}");
}
