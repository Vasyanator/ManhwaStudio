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
        image::save_buffer(&base, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
        let red = ColorImage::filled([10, 10], Color32::from_rgba_unmultiplied(255, 0, 0, 255));
        persist::add_page_raster(
            &layers, None, 0, "r0", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
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
        assert_eq!(&rgba[center..center + 4], &[255, 0, 0, 255], "disk raster composited at center");

        // Migrated layout: primary=unsaved (manifest exists, lacks page 0), raster on committed page 0.
        let committed = dir.join("committed");
        let unsaved = dir.join("unsaved");
        std::fs::create_dir_all(&committed).unwrap();
        std::fs::create_dir_all(&unsaved).unwrap();
        persist::add_page_raster(
            &committed, None, 0, "rc", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
        persist::add_page_raster(
            &unsaved, None, 5, "rs", "R", true, 1.0,
            crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            &red,
        ).unwrap();
        let base2 = dir.join("page2.png");
        image::save_buffer(&base2, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
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
        assert_eq!(&rgba2[center..center + 4], &[255, 0, 0, 255], "committed-only raster composited (migrated)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flatten_composites_raster_from_on_screen_snapshot() {
        // PRIMARY Bug B fix: the export composites the ON-SCREEN raster snapshot (`job.rasters`) even when
        // the disk dirs would yield NOTHING (no `layers.json` at all) — proving the bake no longer depends
        // on a disk re-read that can silently drop the raster.
        let dir = std::env::temp_dir().join(format!("typ_flat_snap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("page.png");
        image::save_buffer(&base, &vec![0u8; 20 * 20 * 4], 20, 20, image::ColorType::Rgba8).unwrap();
        // A 10x10 RED straight-alpha snapshot centered at (10,10), no disk dirs.
        let snap = TypingExportRasterSnapshot {
            visible: true,
            opacity: 1.0,
            transform: crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            rgba: vec![255, 0, 0, 255].repeat(10 * 10),
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
        assert_eq!(&rgba[center..center + 4], &[255, 0, 0, 255], "on-screen snapshot raster composited");
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
            transform: crate::models::layer_model::manifest::TransformRec { cx: 10.0, cy: 10.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            rgba: vec![255, 0, 0, 255].repeat(10 * 10),
            size_px: [10, 10],
            band_z: 0,
            mask_clip_enabled: mask_clip,
        };
        // Page mask ACTIVE only on the LEFT half (x < 10) of the 20x20 page.
        let mask = TypingMaskExportPage {
            width: 20,
            height: 20,
            data: (0..20 * 20).map(|i| if (i % 20) < 10 { 255 } else { 0 }).collect(),
        };
        let make_job = |snap: TypingExportRasterSnapshot, mask: Option<TypingMaskExportPage>| TypingExportPageJob {
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
        };

        // CLIPPED export: left-half page pixels keep the raster (red); right-half are clipped → base (black).
        let (rgba, _, _) = flatten_typing_export_page_rgba(&make_job(make_snap(true), Some(mask.clone()))).unwrap();
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 20 + x) * 4;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        assert_eq!(px(7, 10), [255, 0, 0, 255], "raster kept where mask is active (left half)");
        assert_eq!(px(13, 10), [0, 0, 0, 255], "raster CLIPPED where mask is inactive (right half)");

        // UNCLIPPED (mask_clip OFF): the same right-half pixel keeps the raster.
        let (rgba2, _, _) = flatten_typing_export_page_rgba(&make_job(make_snap(false), Some(mask))).unwrap();
        let i = (10 * 20 + 13) * 4;
        assert_eq!(&rgba2[i..i + 4], &[255, 0, 0, 255], "unclipped raster unchanged on the right half");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_char_budget_floors_at_min_and_grows_with_width() {
        let cp = 8.0; // representative char width px
        // At/below the min available width (5 chars fit) → exactly the min (5).
        assert_eq!(preview_char_budget(5.0 * cp, cp), 5, "5 chars fit → 5");
        assert_eq!(preview_char_budget(0.0, cp), 5, "no room → still min 5");
        assert_eq!(preview_char_budget(-50.0, cp), 5, "negative (overhead > width) → min 5");
        assert_eq!(preview_char_budget(3.0 * cp, cp), 5, "only 3 fit but floor is 5");
        // Grows by 1 per ~char_px wider.
        assert_eq!(preview_char_budget(6.0 * cp, cp), 6, "6 chars wide → 6");
        assert_eq!(preview_char_budget(6.0 * cp + cp / 2.0, cp), 6, "partial char floors down");
        assert_eq!(preview_char_budget(12.0 * cp, cp), 12, "12 chars wide → 12");
        // Degenerate inputs → min (helper guards non-finite available + non-positive char_px).
        assert_eq!(preview_char_budget(1000.0, 0.0), 5, "zero char width → min 5");
        assert_eq!(preview_char_budget(f32::INFINITY, cp), 5, "non-finite available → min 5");
        assert_eq!(preview_char_budget(f32::NAN, cp), 5, "NaN available → min 5");
    }

    #[test]
    fn text_preview_label_appends_dots_to_three_accounting_for_existing() {
        // First `max_chars` CHARACTERS (Unicode), trailing dot-equivalents brought to >= 3 (regular dot
        // = 1, ellipsis '…' = 3), accounting for what's already there. These use max_chars = 5 (the min).
        assert_eq!(text_preview_label("Привет мир", 5), "Приве...", "no trailing dots → append 3");
        assert_eq!(text_preview_label("Да.", 5), "Да...", "1 existing dot → append 2");
        assert_eq!(text_preview_label("Эй..", 5), "Эй...", "2 existing dots → append 1");
        // "Стоп..." → first5 = "Стоп." (С,т,о,п,.), 1 trailing dot → append 2.
        assert_eq!(text_preview_label("Стоп...", 5), "Стоп...", "first-5 truncation keeps one dot → append 2");
        // Ellipsis char counts as 3 → append none.
        assert_eq!(text_preview_label("Всё…", 5), "Всё…", "ellipsis = 3 → append none");
        // "Хм….." → first5 = Х,м,…,.,. → trailing .,. then … = 1+1+3 = 5 → append none.
        assert_eq!(text_preview_label("Хм…..", 5), "Хм…..", "ellipsis + 2 dots → already >= 3");
        // Short text (< 5 chars), not truncated, still gets dots.
        assert_eq!(text_preview_label("Да", 5), "Да...");
        // Empty (after trim) → empty preview (caller shows just "Текст").
        assert_eq!(text_preview_label("", 5), "");
        assert_eq!(text_preview_label("   ", 5), "", "whitespace-only trims to empty");
        // Leading whitespace is trimmed before taking the first 5 chars.
        assert_eq!(text_preview_label("  Привет", 5), "Приве...");
        // Cyrillic char-boundary safety: exactly 5 chars taken, no byte-panic on multibyte text.
        let long = "Текстовая строка";
        assert_eq!(long.chars().count() > 5, true);
        assert_eq!(text_preview_label(long, 5), "Текст...");
        // A 5-char prefix that is ALL dots stays as-is (>= 3).
        assert_eq!(text_preview_label(".....", 5), ".....");

        // Larger max_chars → more preview chars before the dots (wider panel). "Длинноеслово" has no
        // space in the first 10, so the prefix is exactly its first 10 chars.
        assert_eq!(text_preview_label("Длинноеслово", 10), "Длинноесло...", "first 10 chars + dots");
        // A text SHORTER than max_chars still gets the dots.
        assert_eq!(text_preview_label("Привет", 10), "Привет...", "short-than-max still gets dots");
        // Dot accounting still applies with a larger budget.
        assert_eq!(text_preview_label("Конец..", 10), "Конец...", "2 trailing dots → append 1");
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
        assert!(topmost_raster_target(&entries, Some(Pos2::new(900.0, 900.0)), image_rect, None).is_none());

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
        let straight: Vec<u8> = vec![255, 255, 255, 128, 200, 100, 50, 64, 10, 20, 30, 255, 0, 0, 0, 0];
        let image = ColorImage::from_rgba_unmultiplied([4, 1], &straight);
        let out = color_image_to_rgba(&image);
        assert_eq!(out.len(), straight.len());
        // Alpha round-trips exactly; RGB is recovered within the unavoidable premultiply→u8→unpremultiply
        // quantization (≈255/alpha), which the OLD `to_array()` (premultiplied) would blow past entirely.
        for px in 0..4 {
            let a = straight[px * 4 + 3] as i32;
            assert_eq!(out[px * 4 + 3], straight[px * 4 + 3], "alpha exact at pixel {px}");
            // Worst-case round-trip error ≈ ceil(255 / (2*alpha)).
            let tol = if a == 0 { 0 } else { ((255 + 2 * a - 1) / (2 * a)).max(1) };
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
        assert!(out[0] >= 254 && out[1] >= 254 && out[2] >= 254, "white stays white, not premultiplied gray");
    }

    #[test]
    fn image_effects_fx_file_name_appends_fx_suffix() {
        assert_eq!(image_effects_fx_file_name("image_p0_1.png"), "image_p0_1_fx.png");
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
        assert!((tl[0] - (center[0] - size[0] as f32 * 0.5)).abs() < 1e-2, "TL x = cx - w/2");
        assert!((tl[1] - (center[1] - size[1] as f32 * 0.5)).abs() < 1e-2, "TL y = cy - h/2");
        assert!((br[0] - (center[0] + size[0] as f32 * 0.5)).abs() < 1e-2, "BR x = cx + w/2");
        assert!((br[1] - (center[1] + size[1] as f32 * 0.5)).abs() < 1e-2, "BR y = cy + h/2");
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
        assert!((tl_after[0] - (tl_before[0] + 10.0)).abs() < 1e-3, "TL fully follows the drag x");
        assert!((tl_after[1] - (tl_before[1] + 20.0)).abs() < 1e-3, "TL fully follows the drag y");
        assert!((br_after[0] - br_before[0]).abs() < 1e-3, "opposite corner unaffected x");
        assert!((br_after[1] - br_before[1]).abs() < 1e-3, "opposite corner unaffected y");
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
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();

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
        assert_ne!(r0_new, r0_pos, "the reorder actually moved r0 to a new position");
        // All three trackers now point at r0's NEW position (the SAME raster), not the stale index.
        assert_eq!(layer.selected_raster_idx, Some(r0_new), "selection follows r0 by uid");
        assert_eq!(layer.transform_mode_raster_idx, Some(r0_new), "transform mode follows r0 by uid");
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
        assert!(doc.remove_node(0, "r0"));
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.selected_raster_idx, None, "selection cleared when its raster is gone");

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
        persist::write_page_text_payload(&dir, None, 0, &[seed_text("ta", 10.0, 20.0), seed_text("tb", 30.0, 40.0)])
            .unwrap();

        let mut doc = LayerDoc::new();
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        page_sizes.insert(0, [100, 100]);
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();
        assert_eq!(
            doc.page(0).unwrap().nodes.iter().filter(|n| n.is_text()).count(),
            2,
            "doc loaded both text nodes"
        );

        // Migrated-chapter state: NO local overlay runtimes.
        let mut layer = TypingTextOverlayLayer::default();
        assert!(layer.overlays.is_empty());

        layer.sync_from_doc(0, &doc);

        // Both text nodes materialized as runtimes with correct projected fields.
        assert_eq!(layer.overlays.len(), 2, "sync_from_doc created a runtime per doc text node");
        let ta = layer.overlays.iter().find(|o| o.uid == "ta").expect("ta runtime");
        assert_eq!(ta.kind, TypingOverlayKind::Text);
        assert_eq!(ta.page_idx, 0);
        assert_eq!(ta.center_page_px, [10.0, 20.0]);
        assert!((ta.angle_deg - 0.0).abs() < 1e-6);
        assert!((ta.user_scale - 1.0).abs() < 1e-6);
        assert_eq!(ta.layer_idx, 2, "text-group axis carried from the node");
        assert_eq!(ta.size_px, [4, 3], "doc image projected");
        assert_eq!(ta.source_rgba.len(), 4 * 3 * 4, "rgba populated from the doc image");
        assert_eq!(
            ta.file_name,
            persist::text_image_file_name(0, "ta"),
            "deterministic rendered-PNG name (round-trips with the doc flush)"
        );
        assert!(ta.texture.is_none() && ta.display_texture_stale, "queued for upload this frame");
        // Newly-created runtimes are queued for texture upload.
        assert_eq!(layer.pending_upload_indices.len(), 2, "both runtimes queued for upload");

        // A second sync reconciles (no duplicates).
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.overlays.len(), 2, "second sync does NOT duplicate runtimes");

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
        doc.ensure_page_loaded(0, &dir, None, &page_sizes).unwrap();

        let mut layer = TypingTextOverlayLayer::default();
        // 1) Early frame: doc materializes the text runtime (loader still in flight).
        layer.sync_from_doc(0, &doc);
        assert_eq!(layer.overlays.len(), 1, "doc-created the text runtime");

        // 2) Loader completes with an EMPTY decoded set (migrated chapter) — drive the exact merge step
        //    `poll_loader` runs. The doc-created runtime must NOT be wiped.
        let touched = merge_loaded_overlays(&mut layer.overlays, Vec::new());
        assert!(touched.is_empty());
        assert_eq!(layer.overlays.len(), 1, "doc text SURVIVES the empty loader completion (race fixed)");
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
        }
    }

    #[test]
    fn loader_completion_merge_does_not_wipe_doc_created_runtimes() {
        // CRITICAL RACE (the intermittent "text shows then vanishes, sometimes half"): on a MIGRATED
        // chapter `sync_from_doc` materializes text runtimes from the doc on an early frame, then the
        // loader thread completes with an EMPTY decoded set (no `text_info.json`). The old wholesale
        // `self.overlays = decoded` WIPED the doc-created runtimes. The merge must leave them intact.
        let mut overlays: Vec<TypingOverlayRuntime> = vec![
            text_runtime_from_doc_node("ta", 0, [10.0, 20.0], 1.0, 0.0, None, false, 1, None, [4, 3], vec![0u8; 4 * 3 * 4]),
            text_runtime_from_doc_node("tb", 0, [30.0, 40.0], 1.0, 0.0, None, false, 1, None, [4, 3], vec![0u8; 4 * 3 * 4]),
        ];

        // Loader completes with an EMPTY set (migrated chapter).
        let touched = merge_loaded_overlays(&mut overlays, Vec::new());
        assert!(touched.is_empty(), "empty load touches nothing");
        assert_eq!(overlays.len(), 2, "doc-created runtimes SURVIVE an empty loader completion");
        assert!(overlays.iter().any(|o| o.uid == "ta"));
        assert!(overlays.iter().any(|o| o.uid == "tb"));
    }

    #[test]
    fn loader_completion_merge_replaces_same_uid_without_duplicating() {
        // LEGACY/dup case: a doc-created runtime with uid "ta" exists (from the race), and the loader
        // returns the SAME uid "ta" (plus a brand-new "tc"). The merge must REPLACE "ta" in place (no
        // duplicate) and APPEND "tc".
        let mut overlays: Vec<TypingOverlayRuntime> = vec![text_runtime_from_doc_node(
            "ta", 0, [10.0, 20.0], 1.0, 0.0, None, false, 0, None, [4, 3], vec![0u8; 4 * 3 * 4],
        )];

        let touched = merge_loaded_overlays(
            &mut overlays,
            vec![decoded_text_overlay("ta", 0, [99.0, 88.0]), decoded_text_overlay("tc", 0, [1.0, 2.0])],
        );

        assert_eq!(overlays.len(), 2, "same-uid REPLACED in place (no dup), new uid APPENDED");
        let ta = overlays.iter().find(|o| o.uid == "ta").unwrap();
        assert_eq!(ta.center_page_px, [99.0, 88.0], "loaded entry replaced the doc-created one");
        assert_eq!(overlays.iter().filter(|o| o.uid == "ta").count(), 1, "no duplicate ta");
        assert!(overlays.iter().any(|o| o.uid == "tc"), "new loaded overlay appended");
        // Both the replaced and the appended entry are flagged for upload.
        assert_eq!(touched.len(), 2);
        // Same uid on a DIFFERENT page is NOT treated as a match (page-scoped key).
        let mut o2 = vec![text_runtime_from_doc_node(
            "ta", 1, [5.0, 6.0], 1.0, 0.0, None, false, 0, None, [4, 3], vec![0u8; 4 * 3 * 4],
        )];
        merge_loaded_overlays(&mut o2, vec![decoded_text_overlay("ta", 0, [7.0, 8.0])]);
        assert_eq!(o2.len(), 2, "same uid on a different page is a distinct runtime");
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
            font_path: std::path::PathBuf::from("font.ttf"),
            available_inline_fonts: Vec::new(),
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
                row_variants.iter().all(
                    |variant| variant.shape_min_width_percent == params.shape_min_width_percent
                )
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

        let tiles = render_shape_variant_preview_tiles(params, variants, &cancel_render);

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
            text_params.get("hanging_punctuation").and_then(Value::as_bool),
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
        let around = |u: f32, v: f32| {
            Rect::from_center_size(scene_from_uv(page_rect, u, v), Vec2::splat(6.0))
        };
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
