/*
File: page_ops/plan/tests.rs

Purpose:
Unit tests of the planning layer: permutation math per operation, the
placement / split / crop geometry in every coordinate space, page-keyed name
parsers, and `build_plan` over in-memory chapter snapshots (no filesystem).

Notes:
A child of `plan.rs`: `use super::*` reaches the parent's private items; the
functions that live in sibling child modules are imported by path.
*/

use super::*;
use super::crop::resolve_crop;
use super::permutation::permutation_for_op;
use super::split::resolve_split_parts;
use crate::crop_geometry::PageRotation;
use crate::{PageOpError, PageOpKind, StitchPlacement};
use std::path::PathBuf;

fn map_of(perm: &Permutation) -> Vec<Option<usize>> {
    perm.old_to_new.clone()
}

#[test]
fn move_forward_and_backward_permutations() {
    // Move page 0 to the end of a 4-page chapter.
    let perm = permutation_for_op(&PageOpKind::Move { from: 0, to: 3 }, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(3), Some(0), Some(1), Some(2)]);
    assert_eq!(perm.new_page_count, 4);

    // Move page 3 to the front.
    let perm = permutation_for_op(&PageOpKind::Move { from: 3, to: 0 }, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(1), Some(2), Some(3), Some(0)]);

    // Move to the same position is the identity.
    let perm = permutation_for_op(&PageOpKind::Move { from: 2, to: 2 }, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(3)]);

    // Adjacent swap forward: `to` is an index into the order WITHOUT `from`.
    let perm = permutation_for_op(&PageOpKind::Move { from: 1, to: 2 }, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(2), Some(1), Some(3)]);
}

#[test]
fn move_rejects_out_of_range() {
    assert!(matches!(
        permutation_for_op(&PageOpKind::Move { from: 4, to: 0 }, 4),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        permutation_for_op(&PageOpKind::Move { from: 0, to: 4 }, 4),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn insert_at_start_and_end() {
    let files = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.JPG")];
    // At the start: every old page shifts by 2.
    let perm = permutation_for_op(
        &PageOpKind::InsertFiles {
            at: 0,
            files: files.clone(),
        },
        3,
    )
    .expect("valid");
    assert_eq!(map_of(&perm), vec![Some(2), Some(3), Some(4)]);
    assert_eq!(perm.new_page_count, 5);
    assert_eq!(perm.new_pages[0].new_idx, 0);
    assert_eq!(perm.new_pages[1].new_idx, 1);
    // Extensions are lower-cased.
    assert_eq!(perm.new_pages[1].extension, "jpg");

    // At the end: old pages keep their indices.
    let perm = permutation_for_op(&PageOpKind::InsertFiles { at: 3, files }, 3).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2)]);
    assert_eq!(perm.new_pages[0].new_idx, 3);
    assert_eq!(perm.new_pages[1].new_idx, 4);
}

#[test]
fn insert_rejects_empty_list_bad_position_and_bad_extension() {
    assert!(matches!(
        permutation_for_op(&PageOpKind::InsertFiles { at: 0, files: vec![] }, 3),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        permutation_for_op(
            &PageOpKind::InsertFiles {
                at: 4,
                files: vec![PathBuf::from("/tmp/a.png")]
            },
            3
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        permutation_for_op(
            &PageOpKind::InsertFiles {
                at: 0,
                files: vec![PathBuf::from("/tmp/a.webp")]
            },
            3
        ),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn create_blank_validates_dimensions() {
    let ok = permutation_for_op(
        &PageOpKind::CreateBlank {
            at: 1,
            width: 800,
            height: 1200,
            rgba: [255, 255, 255, 255],
        },
        2,
    )
    .expect("valid");
    assert_eq!(map_of(&ok), vec![Some(0), Some(2)]);
    assert_eq!(ok.new_pages[0].extension, "png");

    for (w, h) in [(0, 100), (100, 0), (20_001, 100), (100, 20_001)] {
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::CreateBlank {
                    at: 0,
                    width: w,
                    height: h,
                    rgba: [0, 0, 0, 255],
                },
                2
            ),
            Err(PageOpError::InvalidOp(_))
        ));
    }
}

#[test]
fn delete_multiple_pages() {
    let perm = permutation_for_op(
        &PageOpKind::Delete {
            // Unsorted with a duplicate: the engine sorts and dedups.
            indices: vec![3, 1, 1],
        },
        5,
    )
    .expect("valid");
    assert_eq!(
        map_of(&perm),
        vec![Some(0), None, Some(1), None, Some(2)]
    );
    assert_eq!(perm.new_page_count, 3);
}

#[test]
fn delete_rejects_empty_all_and_out_of_range() {
    assert!(matches!(
        permutation_for_op(&PageOpKind::Delete { indices: vec![] }, 3),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        permutation_for_op(
            &PageOpKind::Delete {
                indices: vec![0, 1, 2]
            },
            3
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        permutation_for_op(&PageOpKind::Delete { indices: vec![3] }, 3),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn page_keyed_name_parsers_roundtrip() {
    assert_eq!(parse_layers_png_page_idx("ps_p0007_ab12.png"), Some(7));
    assert_eq!(
        parse_layers_png_page_idx("ps_p0007_ab12_text.png"),
        Some(7)
    );
    assert_eq!(parse_layers_png_page_idx("ps_p12_x.png"), None);
    assert_eq!(parse_layers_png_page_idx("other.png"), None);

    assert_eq!(parse_typing_mask_page_idx("mask_page_12.png"), Some(12));
    assert_eq!(parse_typing_mask_page_idx("mask_page_.png"), None);
    assert_eq!(parse_typing_mask_page_idx("mask_page_1.jpg"), None);

    assert_eq!(parse_detection_blocks_page_idx("00012_blocks.json"), Some(12));
    assert_eq!(parse_detection_mask_page_idx("00012_mask.png"), Some(12));
    assert_eq!(parse_detection_blocks_page_idx("012_blocks.json"), None);
}

fn snapshot_for_plan() -> ChapterSnapshot {
    let committed = TreeSnapshot {
        tree_rel: "ch1".to_string(),
        clean_overlay_stems: ["000", "002"].iter().map(ToString::to_string).collect(),
        layers_files: ["layers.json", "ps_p0000_u1.png", "ps_p0002_u2_text.png"]
            .iter()
            .map(ToString::to_string)
            .collect(),
        layer_png_sizes: std::collections::BTreeMap::new(),
        layers_manifest: Some(serde_json::json!({
            "schema_version": 3,
            "pages": [
                {"img_idx": 0, "tree": [
                    {"uid": "u1", "name": "L", "z": 0, "visible": true,
                     "opacity": 1.0, "base_file": "ps_p0000_u1.png"}
                ]},
                {"img_idx": 2, "tree": [
                    {"uid": "u2", "name": "T", "z": 0, "visible": true,
                     "opacity": 1.0, "rendered_file": "ps_p0002_u2_text.png"}
                ]}
            ]
        })),
        text_images_files: ["mask_page_1.png", "typing_overlay_p0001_1.png"]
            .iter()
            .map(ToString::to_string)
            .collect(),
        text_info: vec![TextInfoFile {
            location: TextInfoLocation::TextImagesDir,
            entries: vec![serde_json::json!({
                "img_idx": 1, "file": "typing_overlay_p0001_1.png"
            })],
        }],
        bubbles: Some(vec![serde_json::json!({
            "id": 1, "img_idx": 3, "img_u": 0.5, "img_v": 0.5,
            "side": "left", "text": "t", "original_text": "o"
        })]),
        ..TreeSnapshot::default()
    };
    let unsaved = TreeSnapshot {
        tree_rel: "ch1_unsaved".to_string(),
        ..TreeSnapshot::default()
    };
    ChapterSnapshot {
        chapter_rel: "ch1".to_string(),
        page_file_names: vec![
            "000.png".to_string(),
            "001.png".to_string(),
            "002.jpg".to_string(),
            "003.png".to_string(),
        ],
        // Distinct per-page sizes: a mixed-up coordinate space shows up as a
        // wrong number instead of an accidental identity.
        page_sizes: vec![[100, 200], [50, 400], [80, 80], [60, 120]],
        has_alt_vers: false,
        committed,
        unsaved,
        detection: vec![DetectionFiles {
            page_idx: 1,
            blocks: Some(DetectionBlocks::Parsed(serde_json::json!({
                "source_size": [100, 200],
                "blocks": [],
                "mask_file": "00001_mask.png"
            }))),
            has_mask: true,
        }],
    }
}

#[test]
fn build_plan_move_covers_every_category_with_canonical_names() {
    let snapshot = snapshot_for_plan();
    // Move page 0 to the end: 0->3, 1->0, 2->1, 3->2.
    let plan = build_plan(&snapshot, &PageOpKind::Move { from: 0, to: 3 }, 42)
        .expect("plan builds");
    assert_eq!(
        plan.old_to_new,
        vec![Some(3), Some(0), Some(1), Some(2)]
    );

    let final_targets: Vec<(&str, &str)> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
            MoveDest::Trash { .. } | MoveDest::Discard => None,
        })
        .collect();
    // Source pages keep their extension under the new canonical stem.
    assert!(final_targets.contains(&("ch1/src/000.png", "ch1/src/003.png")));
    assert!(final_targets.contains(&("ch1/src/002.jpg", "ch1/src/001.jpg")));
    // Clean overlays follow the page stem.
    assert!(final_targets.contains(&(
        "ch1/clean_layers/000.png",
        "ch1/clean_layers/003.png"
    )));
    assert!(final_targets.contains(&(
        "ch1/clean_layers/002.png",
        "ch1/clean_layers/001.png"
    )));
    // Layer PNGs get the new `ps_p{page:04}_` prefix.
    assert!(final_targets.contains(&(
        "ch1/layers/ps_p0000_u1.png",
        "ch1/layers/ps_p0003_u1.png"
    )));
    assert!(final_targets.contains(&(
        "ch1/layers/ps_p0002_u2_text.png",
        "ch1/layers/ps_p0001_u2_text.png"
    )));
    // Typing mask follows the raw index format.
    assert!(final_targets.contains(&(
        "ch1/text_images/mask_page_1.png",
        "ch1/text_images/mask_page_0.png"
    )));
    // Detection mask follows the 5-digit format.
    assert!(final_targets.contains(&(
        "ch1/text_detection/00001_mask.png",
        "ch1/text_detection/00000_mask.png"
    )));

    // JSON rewrites: bubbles + layers manifest + text_info + blocks.
    let json_targets: Vec<&str> =
        plan.json_writes.iter().map(|w| w.target.as_str()).collect();
    assert!(json_targets.contains(&"ch1/translation_bubbles.json"));
    assert!(json_targets.contains(&"ch1/layers/layers.json"));
    assert!(json_targets.contains(&"ch1/text_images/text_info.json"));
    assert!(json_targets.contains(&"ch1/text_detection/00000_blocks.json"));

    // The rewritten blocks file references the NEW mask name.
    let blocks = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/text_detection/00000_blocks.json")
        .expect("blocks rewrite present");
    assert!(blocks.content.contains("00000_mask.png"));

    // No trash content for a pure move.
    assert!(plan.trash_writes.is_empty());
    assert!(
        plan.moves
            .iter()
            .all(|m| !matches!(m.dest, MoveDest::Trash { .. }))
    );
    // Every temp lives in the same directory as its source.
    for m in &plan.moves {
        let from_dir = m.from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let temp_dir = m.temp.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        assert_eq!(from_dir, temp_dir, "temp of {} in same dir", m.from);
    }
}

#[test]
fn build_plan_delete_moves_artifacts_to_trash() {
    let snapshot = snapshot_for_plan();
    let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 7)
        .expect("plan builds");
    assert_eq!(plan.trash_root, "ch1/.pageop_trash/7");
    let trash_targets: Vec<&str> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Trash { path } => Some(path.as_str()),
            MoveDest::Final { .. } | MoveDest::Discard => None,
        })
        .collect();
    assert!(trash_targets.contains(&"ch1/.pageop_trash/7/ch1/src/001.png"));
    assert!(
        trash_targets.contains(&"ch1/.pageop_trash/7/ch1/text_images/mask_page_1.png")
    );
    assert!(trash_targets
        .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_blocks.json"));
    assert!(trash_targets
        .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_mask.png"));
    // The deleted page's overlay PNG (referenced by its text_info entry)
    // is archived too.
    assert!(trash_targets
        .contains(&"ch1/.pageop_trash/7/ch1/text_images/typing_overlay_p0001_1.png"));
    // Deleted text_info entries are archived.
    assert!(plan.trash_writes.iter().any(|w| w.target
        == "ch1/.pageop_trash/7/ch1/text_images/deleted_text_info.json"));
}

#[test]
fn delete_keeps_text_overlay_referenced_by_surviving_entry() {
    let mut snapshot = snapshot_for_plan();
    snapshot.committed.text_info[0].entries = vec![
        serde_json::json!({"img_idx": 1, "file": "shared.png"}),
        serde_json::json!({"img_idx": 2, "file": "shared.png"}),
    ];
    snapshot.committed.text_images_files.insert("shared.png".to_string());
    let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 8)
        .expect("plan builds");
    assert!(!plan.moves.iter().any(|planned| {
        planned.from == "ch1/text_images/shared.png"
            && matches!(planned.dest, MoveDest::Trash { .. })
    }));
}

/// A placement with no crop and no scale, for permutation-only tests.
fn whole_page(page_idx: usize, dx: i64, dy: i64, size: [u32; 2]) -> StitchPlacement {
    StitchPlacement {
        page_idx,
        crop: [0, 0, size[0], size[1]],
        scale: 1.0,
        dx,
        dy,
    }
}

fn stitch_op(placements: Vec<StitchPlacement>, width: u32, height: u32) -> PageOpKind {
    PageOpKind::Stitch {
        placements,
        width,
        height,
        background: [0, 0, 0, 0],
    }
}

#[test]
fn stitch_folds_sources_onto_the_lowest_index() {
    // Pages 1 and 3 of 5 merge: 0 stays, 1+3 -> 1, 2 -> 2, 4 -> 3.
    let perm = permutation_for_op(
        &stitch_op(
            vec![
                whole_page(3, 0, 0, [50, 400]),
                whole_page(1, 0, 0, [50, 400]),
            ],
            100,
            400,
        ),
        5,
    )
    .expect("valid");
    assert_eq!(
        map_of(&perm),
        vec![Some(0), Some(1), Some(2), Some(1), Some(3)]
    );
    assert_eq!(perm.new_page_count, 4);
    // The merged page is not an "inserted" page: its content comes from the
    // chapter snapshot, not from the request.
    assert!(perm.new_pages.is_empty());

    // Merging the first two pages of 3 leaves 2 pages, primary at index 0.
    let perm = permutation_for_op(
        &stitch_op(
            vec![whole_page(0, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
            20,
            10,
        ),
        3,
    )
    .expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(0), Some(1)]);
    assert_eq!(perm.new_page_count, 2);
}

#[test]
fn stitch_rejects_bad_selections_and_canvases() {
    // Fewer than two pages.
    assert!(matches!(
        permutation_for_op(&stitch_op(vec![whole_page(0, 0, 0, [10, 10])], 10, 10), 3),
        Err(PageOpError::InvalidOp(_))
    ));
    // Duplicate page.
    assert!(matches!(
        permutation_for_op(
            &stitch_op(
                vec![whole_page(1, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
                20,
                10
            ),
            3
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    // Out of range.
    assert!(matches!(
        permutation_for_op(
            &stitch_op(
                vec![whole_page(0, 0, 0, [10, 10]), whole_page(3, 0, 0, [10, 10])],
                20,
                10
            ),
            3
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    // Canvas out of bounds (zero, too wide, too many pixels).
    for (w, h) in [(0, 10), (10, 0), (STITCH_MAX_SIDE_PX + 1, 10), (39_000, 39_000)] {
        assert!(
            matches!(
                permutation_for_op(
                    &stitch_op(
                        vec![whole_page(0, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
                        w,
                        h
                    ),
                    3
                ),
                Err(PageOpError::InvalidOp(_))
            ),
            "canvas {w}x{h} must be rejected"
        );
    }
}

#[test]
fn placement_map_maps_every_coordinate_space() {
    // A 100x200 page, cropped to its lower-right 60x150 quadrant, doubled,
    // and placed at (40, 10) of a 400x400 canvas.
    let placement = StitchPlacement {
        page_idx: 0,
        crop: [20, 40, 60, 150],
        scale: 2.0,
        dx: 40,
        dy: 10,
    };
    let map = PlacementMap::new(&placement, [100, 200], [400, 400]).expect("valid");
    assert_eq!(map.placed_rect(), [40, 10, 120, 300]);
    assert_eq!(map.crop_rect(), [20, 40, 60, 150]);
    assert!(!map.rotates());
    // Absolute page px -> canvas px. The crop origin maps to (dx, dy).
    let (x, y) = map.map_point(20.0, 40.0);
    assert!((x - 40.0).abs() < 1e-9 && (y - 10.0).abs() < 1e-9);
    let (x, y) = map.map_point(70.0, 140.0);
    assert!((x - 140.0).abs() < 1e-9 && (y - 210.0).abs() < 1e-9);
    // Magnitudes carry the scale but not the origin.
    assert!((map.map_extent(3.0) - 6.0).abs() < 1e-9);
    // Page-normalized uv -> canvas-normalized uv: u=0.2 is page px 20,
    // which is the crop origin, i.e. canvas px 40 = 0.1 of a 400 canvas;
    // v=0.2 is page px 40 -> canvas px 10 -> 0.025.
    let (u, v) = map.map_uv(0.2, 0.2);
    assert!((u - 0.1).abs() < 1e-9);
    assert!((v - 0.025).abs() < 1e-9);
    // A non-rotating placement maps one axis without the other, exactly.
    assert_eq!(map.map_x_without_y(20.0), Some(40.0));
    assert_eq!(map.map_y_without_x(40.0), Some(10.0));
    let lone_u = map.map_u_without_v(0.2).expect("separable without rotation");
    assert!((lone_u - 0.1).abs() < 1e-9);
    let lone_v = map.map_v_without_u(0.2).expect("separable without rotation");
    assert!((lone_v - 0.025).abs() < 1e-9);
    // An axis-aligned rect keeps its two given corners, inverted or not.
    assert_eq!(map.map_px_rect([70.0, 140.0, 20.0, 40.0]), [
        140.0, 210.0, 40.0, 10.0
    ]);
}

#[test]
fn placement_map_rotation_is_exact_for_a_quarter_turn() {
    // The same 100x200 page turned 90 deg clockwise: its rotated canvas is
    // 200x100, and the crop is read in THAT space.
    let placement = StitchPlacement {
        page_idx: 0,
        crop: [20, 10, 60, 50],
        scale: 1.0,
        dx: 0,
        dy: 0,
    };
    let rotation = PageRotation::new(1, 0.0).expect("valid rotation");
    let map = PlacementMap::with_rotation(&placement, [100, 200], rotation, [60, 50])
        .expect("valid");
    assert!(map.rotates());
    assert_eq!(map.crop_rect(), [20, 10, 60, 50]);
    assert_eq!(map.placed_rect(), [0, 0, 60, 50]);
    // Page (0, 0) is canvas (200, 0) after the turn, i.e. (180, -10) after
    // the crop — outside the part, which is legal for a mapped point.
    assert_eq!(map.map_point(0.0, 0.0), (180.0, -10.0));
    // Page (30, 30) -> turned (170, 30) -> cropped (150, 20). Exact.
    assert_eq!(map.map_point(30.0, 30.0), (150.0, 20.0));
    // One axis alone is no longer defined.
    assert_eq!(map.map_x_without_y(30.0), None);
    assert_eq!(map.map_y_without_x(30.0), None);
    assert_eq!(map.map_u_without_v(0.5), None);
    assert_eq!(map.map_v_without_u(0.5), None);
    // An axis-aligned rect becomes the bounding box of the mapped quad,
    // normalized: the turn swaps and mirrors the corners.
    assert_eq!(map.map_px_rect([30.0, 30.0, 40.0, 50.0]), [
        130.0, 20.0, 150.0, 30.0
    ]);
    // A crop leaving the ROTATED canvas is refused (200x100, not 100x200).
    let mut bad = placement;
    bad.crop = [20, 150, 60, 50];
    assert!(matches!(
        PlacementMap::with_rotation(&bad, [100, 200], rotation, [60, 50]),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn placement_map_rejects_geometry_that_does_not_fit() {
    let base = StitchPlacement {
        page_idx: 2,
        crop: [0, 0, 100, 200],
        scale: 1.0,
        dx: 0,
        dy: 0,
    };
    // Crop leaving the page.
    let mut bad = base;
    bad.crop = [50, 0, 100, 200];
    assert!(matches!(
        PlacementMap::new(&bad, [100, 200], [400, 400]),
        Err(PageOpError::InvalidOp(_))
    ));
    // Empty crop.
    let mut bad = base;
    bad.crop = [0, 0, 0, 200];
    assert!(matches!(
        PlacementMap::new(&bad, [100, 200], [400, 400]),
        Err(PageOpError::InvalidOp(_))
    ));
    // Scale out of range, non-finite, or zero.
    for scale in [0.0, -1.0, f32::NAN, STITCH_MAX_SCALE + 0.5] {
        let mut bad = base;
        bad.scale = scale;
        assert!(
            matches!(
                PlacementMap::new(&bad, [100, 200], [400, 400]),
                Err(PageOpError::InvalidOp(_))
            ),
            "scale {scale} must be rejected"
        );
    }
    // Placed rect leaving the canvas (negative and overflowing).
    let mut bad = base;
    bad.dx = -1;
    assert!(matches!(
        PlacementMap::new(&bad, [100, 200], [400, 400]),
        Err(PageOpError::InvalidOp(_))
    ));
    let mut bad = base;
    bad.dy = 201;
    assert!(matches!(
        PlacementMap::new(&bad, [100, 200], [400, 400]),
        Err(PageOpError::InvalidOp(_))
    ));
    // A zero-sized page has no usable geometry.
    assert!(matches!(
        PlacementMap::new(&base, [0, 200], [400, 400]),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn build_plan_stitch_composes_rasters_and_merges_documents() {
    let snapshot = snapshot_for_plan();
    // Merge pages 0 (100x200) and 2 (80x80) onto a 180x200 canvas.
    let op = stitch_op(
        vec![
            whole_page(0, 0, 0, [100, 200]),
            whole_page(2, 100, 0, [80, 80]),
        ],
        180,
        200,
    );
    let plan = build_plan(&snapshot, &op, 55).expect("plan builds");
    assert_eq!(plan.old_to_new, vec![Some(0), Some(1), Some(0), Some(2)]);
    assert_eq!(plan.new_page_count, 3);

    // Both source pages go to the trash; nothing is destroyed.
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Trash { path } => Some(path.as_str()),
            MoveDest::Final { .. } | MoveDest::Discard => None,
        })
        .collect();
    assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/src/000.png"));
    assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/src/002.jpg"));
    assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/clean_layers/000.png"));
    assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/clean_layers/002.png"));

    // The merged page is a composed PNG of both sources, in page order.
    let create = plan
        .creates
        .iter()
        .find(|c| c.target == "ch1/src/000.png")
        .expect("composed page staged");
    let NewPageContent::ComposedPng {
        width,
        height,
        sources,
        ..
    } = &create.content
    else {
        panic!("stitched page must be a composed PNG");
    };
    assert_eq!((*width, *height), (180, 200));
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].path, "ch1/src/000.png");
    assert_eq!(sources[0].dest, [0, 0, 100, 200]);
    assert_eq!(sources[1].path, "ch1/src/002.jpg");
    assert_eq!(sources[1].dest, [100, 0, 80, 80]);
    // Clean overlays of both sources compose onto ONE transparent overlay.
    let clean = plan
        .creates
        .iter()
        .find(|c| c.target == "ch1/clean_layers/000.png")
        .expect("composed clean overlay staged");
    let NewPageContent::ComposedPng {
        background,
        sources,
        ..
    } = &clean.content
    else {
        panic!("composed overlay expected");
    };
    assert_eq!(*background, [0, 0, 0, 0]);
    assert_eq!(sources.len(), 2);

    // Surviving pages compact around the merged one.
    let finals: Vec<(&str, &str)> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
            MoveDest::Trash { .. } | MoveDest::Discard => None,
        })
        .collect();
    // Page 1 keeps index 1: an identity rename is never planned.
    assert!(!finals.iter().any(|(from, _)| *from == "ch1/src/001.png"));
    assert!(finals.contains(&("ch1/src/003.png", "ch1/src/002.png")));
    // Layer PNGs of both merged pages now share the primary's prefix.
    assert!(finals.contains(&(
        "ch1/layers/ps_p0002_u2_text.png",
        "ch1/layers/ps_p0000_u2_text.png"
    )));

    // The two manifest pages became one entry at the merged index.
    let manifest = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/layers/layers.json")
        .expect("manifest rewritten");
    let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
    let pages = manifest["pages"].as_array().expect("pages");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["img_idx"], serde_json::json!(0));
    assert_eq!(pages[0]["tree"].as_array().expect("tree").len(), 2);
}

#[test]
fn build_plan_stitch_warns_about_position_matched_alt_versions() {
    let mut snapshot = snapshot_for_plan();
    snapshot.has_alt_vers = true;
    let op = stitch_op(
        vec![
            whole_page(0, 0, 0, [100, 200]),
            whole_page(2, 100, 0, [80, 80]),
        ],
        180,
        200,
    );
    let plan = build_plan(&snapshot, &op, 58).expect("plan builds");
    assert!(
        plan.warnings.iter().any(|w| w.contains("alt_vers")),
        "merging pages must warn about the alt-version misalignment: {:?}",
        plan.warnings
    );
    // A chapter without alternate versions stays silent.
    snapshot.has_alt_vers = false;
    let plan = build_plan(&snapshot, &op, 59).expect("plan builds");
    assert!(!plan.warnings.iter().any(|w| w.contains("alt_vers")));
}

#[test]
fn build_plan_stitch_refuses_pages_sharing_a_layer_uid() {
    let mut snapshot = snapshot_for_plan();
    // Page 2 carries the same uid as page 0: both PNGs would become
    // `ps_p0000_u1.png`.
    snapshot
        .committed
        .layers_files
        .insert("ps_p0002_u1.png".to_string());
    let op = stitch_op(
        vec![
            whole_page(0, 0, 0, [100, 200]),
            whole_page(2, 100, 0, [80, 80]),
        ],
        180,
        200,
    );
    let err = build_plan(&snapshot, &op, 56).expect_err("uid collision must be refused");
    assert!(matches!(err, PageOpError::InvalidOp(_)), "got: {err}");
}

#[test]
fn build_plan_stitch_needs_page_sizes() {
    let mut snapshot = snapshot_for_plan();
    snapshot.page_sizes.clear();
    let op = stitch_op(
        vec![
            whole_page(0, 0, 0, [100, 200]),
            whole_page(2, 100, 0, [80, 80]),
        ],
        180,
        200,
    );
    assert!(matches!(
        build_plan(&snapshot, &op, 57),
        Err(PageOpError::InvalidOp(_))
    ));
}

// -----------------------------------------------------------------------
// Split.
// -----------------------------------------------------------------------

/// A split request that keeps every part.
fn split_op(page_idx: usize, axis: SplitAxis, cuts: &[u32], order: &[usize]) -> PageOpKind {
    let deleted = vec![false; order.len()];
    split_op_deleted(page_idx, axis, cuts, order, &deleted)
}

/// A split request with an explicit per-part deletion flag.
fn split_op_deleted(
    page_idx: usize,
    axis: SplitAxis,
    cuts: &[u32],
    order: &[usize],
    deleted: &[bool],
) -> PageOpKind {
    PageOpKind::Split {
        page_idx,
        axis,
        cuts: cuts.to_vec(),
        order: order.to_vec(),
        deleted: deleted.to_vec(),
    }
}

#[test]
fn split_fans_one_page_into_a_contiguous_run_of_parts() {
    // Page 1 of 4 cut into 3 parts: pages 2 and 3 shift up by 2.
    let perm = permutation_for_op(
        &split_op(1, SplitAxis::Horizontal, &[10, 20], &[0, 1, 2]),
        4,
    )
    .expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(4), Some(5)]);
    assert_eq!(perm.new_page_count, 6);
    // The parts are not "inserted" pages: their pixels are cut out of the
    // chapter snapshot, not derivable from the request.
    assert!(perm.new_pages.is_empty());

    // D1: the representative index is the SOURCE index whatever order the
    // user chose — the part ordered first is the one that lands there.
    let perm =
        permutation_for_op(&split_op(2, SplitAxis::Vertical, &[10], &[1, 0]), 4)
            .expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(4)]);
    assert_eq!(perm.new_page_count, 5);
}

#[test]
fn split_deleted_parts_consume_no_index_and_compact_the_survivors() {
    // Page 1 of 4 cut into 3 parts with the MIDDLE one deleted: two parts
    // survive, so the later pages shift up by 1, not by 2.
    let op = split_op_deleted(
        1,
        SplitAxis::Horizontal,
        &[10, 20],
        &[0, 1, 2],
        &[false, true, false],
    );
    let perm = permutation_for_op(&op, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(3), Some(4)]);
    assert_eq!(perm.new_page_count, 5);

    // Several parts deleted at once, and the survivors keep the RELATIVE
    // order the user gave them: part 3 is ordered before part 0.
    let op = split_op_deleted(
        0,
        SplitAxis::Vertical,
        &[10, 20, 30],
        &[3, 1, 2, 0],
        &[false, true, true, false],
    );
    let perm = permutation_for_op(&op, 3).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(2), Some(3)]);
    assert_eq!(perm.new_page_count, 4);
    let geo = resolve_split_parts(
        0,
        SplitAxis::Vertical,
        [40, 10],
        &[10, 20, 30],
        &[3, 1, 2, 0],
        &[false, true, true, false],
    )
    .expect("valid");
    // Part 3 was ordered first, so it takes the source index; part 0
    // follows it. The deleted parts have no index at all.
    assert_eq!(geo.part_new_idx(3), Some(0));
    assert_eq!(geo.part_new_idx(0), Some(1));
    assert_eq!(geo.part_new_idx(1), None);
    assert_eq!(geo.part_new_idx(2), None);
    assert!(geo.is_deleted_part(1) && geo.is_deleted_part(2));
    assert!(!geo.is_deleted_part(3));
    assert_eq!(geo.kept_count(), 2);
    // The fallback target for anything unroutable is a SURVIVING part.
    assert_eq!(geo.first_kept_part(), 3);
    assert_eq!(geo.first_kept_new_idx(), 0);

    // D3: keeping exactly one part is a crop — the page count is unchanged
    // and the shift is zero.
    let op = split_op_deleted(1, SplitAxis::Horizontal, &[10], &[0, 1], &[false, true]);
    let perm = permutation_for_op(&op, 4).expect("valid");
    assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(3)]);
    assert_eq!(perm.new_page_count, 4);
}

#[test]
fn split_rejects_deleting_every_part_and_bad_deletion_flags() {
    for (case, op) in [
        (
            "every part deleted",
            split_op_deleted(1, SplitAxis::Horizontal, &[10], &[0, 1], &[true, true]),
        ),
        (
            "deletion flags too short",
            split_op_deleted(1, SplitAxis::Horizontal, &[10], &[0, 1], &[false]),
        ),
        (
            "deletion flags too long",
            split_op_deleted(
                1,
                SplitAxis::Horizontal,
                &[10],
                &[0, 1],
                &[false, false, false],
            ),
        ),
    ] {
        assert!(
            matches!(permutation_for_op(&op, 4), Err(PageOpError::InvalidOp(_))),
            "{case} must be rejected"
        );
    }
    // The geometry pass validates the same request independently and must
    // agree with the index pass about what is legal.
    assert!(matches!(
        resolve_split_parts(
            1,
            SplitAxis::Horizontal,
            [50, 400],
            &[100],
            &[0, 1],
            &[true, true]
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    assert!(matches!(
        resolve_split_parts(1, SplitAxis::Horizontal, [50, 400], &[100], &[0, 1], &[true]),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn split_rejects_bad_pages_cuts_and_orders() {
    for (case, op) in [
        ("page out of range", split_op(4, SplitAxis::Horizontal, &[10], &[0, 1])),
        ("no cuts", split_op(1, SplitAxis::Horizontal, &[], &[0])),
        (
            "cuts not strictly increasing",
            split_op(1, SplitAxis::Horizontal, &[10, 10], &[0, 1, 2]),
        ),
        ("order too short", split_op(1, SplitAxis::Horizontal, &[10], &[0])),
        (
            "duplicated order position",
            split_op(1, SplitAxis::Horizontal, &[10], &[0, 0]),
        ),
        (
            "order position out of range",
            split_op(1, SplitAxis::Horizontal, &[10], &[0, 2]),
        ),
    ] {
        assert!(
            matches!(
                permutation_for_op(&op, 4),
                Err(PageOpError::InvalidOp(_))
            ),
            "{case} must be rejected"
        );
    }
    // Too many parts.
    let cuts: Vec<u32> = (1..=u32::try_from(SPLIT_MAX_PARTS).expect("fits")).collect();
    let order: Vec<usize> = (0..cuts.len() + 1).collect();
    assert!(matches!(
        permutation_for_op(&split_op(1, SplitAxis::Horizontal, &cuts, &order), 4),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn split_parts_map_every_coordinate_space_and_reject_empty_parts() {
    // A 50x400 page cut horizontally at 100 and 250, parts kept in order.
    let geo = resolve_split_parts(
        1,
        SplitAxis::Horizontal,
        [50, 400],
        &[100, 250],
        &[0, 1, 2],
        &[false, false, false],
    )
    .expect("valid");
    assert_eq!(geo.part_count(), 3);
    assert_eq!(geo.part_size(0), Some([50, 100]));
    assert_eq!(geo.part_size(1), Some([50, 150]));
    assert_eq!(geo.part_size(2), Some([50, 150]));
    assert_eq!(geo.part_new_idx(0), Some(1));
    assert_eq!(geo.part_new_idx(2), Some(3));
    let middle = geo.placement(1).expect("middle part");
    assert_eq!(middle.crop_rect(), [0, 100, 50, 150]);
    assert_eq!(middle.placed_rect(), [0, 0, 50, 150]);
    // Absolute page px: the first cut is the top of the middle part.
    assert!(middle.map_point(0.0, 100.0).1.abs() < 1e-9);
    assert!((middle.map_point(0.0, 175.0).1 - 75.0).abs() < 1e-9);
    // Page-normalized v renormalizes onto the part; u is untouched by a
    // horizontal cut (the part is as wide as the page).
    let (u, v) = middle.map_uv(0.5, 0.25);
    assert!(v.abs() < 1e-9);
    assert!((u - 0.5).abs() < 1e-9);
    // A split never resamples, so page-px magnitudes pass through.
    assert!((middle.map_extent(7.0) - 7.0).abs() < 1e-9);

    // The transpose: a vertical cut renormalizes u and leaves v alone.
    let geo = resolve_split_parts(0, SplitAxis::Vertical, [50, 400], &[20], &[1, 0], &[false, false])
        .expect("valid");
    assert_eq!(geo.part_size(0), Some([20, 400]));
    assert_eq!(geo.part_size(1), Some([30, 400]));
    // Reversed order: the LEFT part goes second, the right one first.
    assert_eq!(geo.part_new_idx(0), Some(1));
    assert_eq!(geo.part_new_idx(1), Some(0));
    let right = geo.placement(1).expect("right part");
    assert!(right.map_point(20.0, 0.0).0.abs() < 1e-9);
    assert!((right.map_uv(0.0, 0.25).1 - 0.25).abs() < 1e-9);

    // Cuts on or past the borders would produce a zero-sized part.
    for cut in [0u32, 400] {
        assert!(
            matches!(
                resolve_split_parts(
                    1,
                    SplitAxis::Horizontal,
                    [50, 400],
                    &[cut],
                    &[0, 1],
                    &[false, false],
                ),
                Err(PageOpError::InvalidOp(_))
            ),
            "cut at {cut} must be rejected"
        );
    }
    assert!(matches!(
        resolve_split_parts(
            1,
            SplitAxis::Horizontal,
            [50, 400],
            &[100, 100],
            &[0, 1, 2],
            &[false, false, false],
        ),
        Err(PageOpError::InvalidOp(_))
    ));
    // A page with no pixels has no geometry at all.
    assert!(matches!(
        resolve_split_parts(1, SplitAxis::Horizontal, [50, 0], &[10], &[0, 1], &[false, false]),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn split_assignment_uses_exact_area_with_a_top_left_tie_break() {
    // A 100x400 page cut in half horizontally.
    let geo = resolve_split_parts(0, SplitAxis::Horizontal, [100, 400], &[200], &[0, 1], &[false, false])
        .expect("valid");
    let rect = |top: f64, bottom: f64| {
        [[10.0, top], [90.0, top], [90.0, bottom], [10.0, bottom]]
    };
    assert_eq!(geo.part_for_polygon(&rect(220.0, 300.0)), Some(1));
    // 75% above the cut.
    assert_eq!(geo.part_for_polygon(&rect(140.0, 220.0)), Some(0));
    // Exactly half on each side: the TOP part wins (geometric position,
    // not user order).
    assert_eq!(geo.part_for_polygon(&rect(150.0, 250.0)), Some(0));
    // A right triangle that is WIDE AT THE BOTTOM: its bounding box spans
    // the whole page (a tie a bbox rule would resolve to the top part),
    // but three quarters of its real area sit below the cut.
    assert_eq!(
        geo.part_for_polygon(&[[0.0, 0.0], [0.0, 400.0], [100.0, 400.0]]),
        Some(1)
    );
    // A polygon enclosing no area has no majority at all.
    assert_eq!(
        geo.part_for_polygon(&[[10.0, 10.0], [90.0, 10.0], [50.0, 10.0]]),
        None
    );
    // Points follow the part CROP rectangles: a point on the cut line
    // belongs to the part that owns that pixel row.
    assert_eq!(geo.part_for_point(50.0, 199.9), 0);
    assert_eq!(geo.part_for_point(50.0, 200.0), 1);
    // Outside the page, the extreme parts claim the point.
    assert_eq!(geo.part_for_point(50.0, -20.0), 0);
    assert_eq!(geo.part_for_point(50.0, 900.0), 1);

    // The transpose: an exact tie across a vertical cut goes LEFT.
    let geo = resolve_split_parts(0, SplitAxis::Vertical, [400, 100], &[200], &[0, 1], &[false, false])
        .expect("valid");
    assert_eq!(
        geo.part_for_polygon(&[
            [150.0, 10.0],
            [250.0, 10.0],
            [250.0, 90.0],
            [150.0, 90.0]
        ]),
        Some(0)
    );
}

/// The plan snapshot with page 1 (50x400) carrying a full layer stack, a
/// clean overlay, a typing mask and detection data, so a split of that page
/// has something to partition in every category.
fn snapshot_for_split() -> ChapterSnapshot {
    let mut snapshot = snapshot_for_plan();
    snapshot.committed.clean_overlay_stems.insert("001".to_string());
    for name in ["ps_p0001_top.png", "ps_p0001_bottom.png", "ps_p0001_text.png"] {
        snapshot.committed.layers_files.insert(name.to_string());
    }
    // Only the TEXT node needs a probe: a raster stores its `image_size`.
    snapshot
        .committed
        .layer_png_sizes
        .insert("ps_p0001_text.png".to_string(), [20, 20]);
    snapshot.committed.layers_manifest = Some(serde_json::json!({
        "schema_version": 4,
        "pages": [
            {"img_idx": 1,
             "groups": [{"uid": "g1", "name": "G", "visible": true, "opacity": 1.0}],
             "text_groups": [{"layer_idx": 0, "z": 1, "name": "TG"}],
             "tree": [
                {"uid": "top", "name": "Top", "kind": "raster", "z": 0,
                 "visible": true, "opacity": 1.0, "group_uid": "g1",
                 "base_file": "ps_p0001_top.png", "image_size": [40, 40],
                 "transform": {"cx": 25.0, "cy": 50.0, "rotation": 0.0, "scale": 1.0}},
                {"uid": "text", "name": "T", "kind": "text", "z": 1, "layer_idx": 0,
                 "visible": true, "opacity": 1.0,
                 "rendered_file": "ps_p0001_text.png",
                 "transform": {"cx": 25.0, "cy": 300.0, "rotation": 0.0, "scale": 1.0}},
                {"uid": "bottom", "name": "Bottom", "kind": "raster", "z": 2,
                 "visible": true, "opacity": 1.0, "group_uid": "g1",
                 "base_file": "ps_p0001_bottom.png", "image_size": [40, 40],
                 "transform": {"cx": 25.0, "cy": 350.0, "rotation": 0.0, "scale": 1.0}}
             ]},
            {"img_idx": 2, "tree": [
                {"uid": "u2", "name": "T", "z": 0, "visible": true,
                 "opacity": 1.0, "rendered_file": "ps_p0002_u2_text.png"}
            ]}
        ]
    }));
    snapshot.detection = vec![DetectionFiles {
        page_idx: 1,
        blocks: Some(DetectionBlocks::Parsed(serde_json::json!({
            "page_idx": 1,
            "source_size": [50, 400],
            "mask_size": [50, 400],
            "blocks": [
                {"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 40.0},
                {"x1": 5.0, "y1": 300.0, "x2": 9.0, "y2": 380.0}
            ],
            "mask_file": "00001_mask.png"
        }))),
        has_mask: true,
    }];
    snapshot
}

#[test]
fn build_plan_split_cuts_rasters_and_partitions_documents() {
    let snapshot = snapshot_for_split();
    // Cut page 1 (50x400) in half; the top part keeps index 1.
    let plan = build_plan(
        &snapshot,
        &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
        77,
    )
    .expect("plan builds");
    assert_eq!(plan.old_to_new, vec![Some(0), Some(1), Some(3), Some(4)]);
    assert_eq!(plan.new_page_count, 5);

    // Both parts are staged as PNGs cut out of the source page.
    let cut_source = |target: &str, crop: [u32; 4], size: (u32, u32)| {
        let create = plan
            .creates
            .iter()
            .find(|c| c.target == target)
            .unwrap_or_else(|| panic!("{target} staged"));
        let NewPageContent::ComposedPng {
            width,
            height,
            sources,
            ..
        } = &create.content
        else {
            panic!("{target} must be a composed PNG");
        };
        assert_eq!((*width, *height), size);
        assert_eq!(sources.len(), 1, "a split part has exactly one source");
        assert_eq!(sources[0].crop, crop);
        // Crop == destination, so `encode_composed_png` copies the pixels
        // instead of resampling.
        assert_eq!(sources[0].dest, [0, 0, crop[2], crop[3]]);
        sources[0].path.clone()
    };
    assert_eq!(
        cut_source("ch1/src/001.png", [0, 0, 50, 200], (50, 200)),
        "ch1/src/001.png"
    );
    assert_eq!(
        cut_source("ch1/src/002.png", [0, 200, 50, 200], (50, 200)),
        "ch1/src/001.png"
    );
    cut_source("ch1/clean_layers/001.png", [0, 0, 50, 200], (50, 200));
    cut_source("ch1/clean_layers/002.png", [0, 200, 50, 200], (50, 200));
    cut_source("ch1/text_images/mask_page_1.png", [0, 0, 50, 200], (50, 200));
    cut_source("ch1/text_images/mask_page_2.png", [0, 200, 50, 200], (50, 200));
    cut_source("ch1/text_detection/00001_mask.png", [0, 0, 50, 200], (50, 200));
    cut_source("ch1/text_detection/00002_mask.png", [0, 200, 50, 200], (50, 200));

    // The originals are recoverable, never destroyed.
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Trash { path } => Some(path.as_str()),
            MoveDest::Final { .. } | MoveDest::Discard => None,
        })
        .collect();
    assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/src/001.png"));
    assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/clean_layers/001.png"));
    assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/text_images/mask_page_1.png"));

    // Layer PNGs of ONE page fan out onto DIFFERENT prefixes.
    let finals: Vec<(&str, &str)> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
            MoveDest::Trash { .. } | MoveDest::Discard => None,
        })
        .collect();
    // `top` stays on part 0, which keeps index 1: an identity rename is
    // never planned.
    assert!(!finals.iter().any(|(from, _)| *from == "ch1/layers/ps_p0001_top.png"));
    assert!(finals.contains(&(
        "ch1/layers/ps_p0001_bottom.png",
        "ch1/layers/ps_p0002_bottom.png"
    )));
    assert!(finals.contains(&(
        "ch1/layers/ps_p0001_text.png",
        "ch1/layers/ps_p0002_text.png"
    )));
    // The untouched pages compact around the parts.
    assert!(finals.contains(&("ch1/src/002.jpg", "ch1/src/003.jpg")));
    assert!(finals.contains(&(
        "ch1/layers/ps_p0002_u2_text.png",
        "ch1/layers/ps_p0003_u2_text.png"
    )));

    // The manifest page entry became TWO entries, one per part.
    let manifest = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/layers/layers.json")
        .expect("manifest rewritten");
    let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
    let pages = manifest["pages"].as_array().expect("pages");
    let by_idx = |idx: u64| {
        pages
            .iter()
            .find(|page| page["img_idx"] == serde_json::json!(idx))
            .unwrap_or_else(|| panic!("page {idx} present"))
    };
    let top = by_idx(1);
    assert_eq!(top["tree"].as_array().expect("tree").len(), 1);
    assert_eq!(top["tree"][0]["uid"], serde_json::json!("top"));
    // Geometry mapped into the part: cy 50 stays 50 in the top part.
    assert_eq!(top["tree"][0]["transform"]["cy"], serde_json::json!(50.0));
    // The PS group is duplicated into every part holding a member.
    assert_eq!(top["groups"].as_array().expect("groups").len(), 1);
    // No unpinned text node here, so the text-group band is dropped.
    assert!(top.get("text_groups").is_none());

    let bottom = by_idx(2);
    assert_eq!(bottom["tree"].as_array().expect("tree").len(), 2);
    // cy 300 of the source page is cy 100 of the bottom part.
    assert_eq!(bottom["tree"][0]["transform"]["cy"], serde_json::json!(100.0));
    assert_eq!(bottom["tree"][1]["transform"]["cy"], serde_json::json!(150.0));
    // z is re-ranked densely PER PART: the source bands 1 and 2 become 0
    // and 1, and the text group keeps sharing the text node's band.
    assert_eq!(bottom["tree"][0]["z"], serde_json::json!(0));
    assert_eq!(bottom["tree"][1]["z"], serde_json::json!(1));
    assert_eq!(bottom["text_groups"][0]["z"], serde_json::json!(0));
    assert_eq!(bottom["groups"].as_array().expect("groups").len(), 1);
    // Page 2's untouched entry followed the ordinary index shift.
    assert_eq!(
        by_idx(3)["tree"][0]["rendered_file"],
        serde_json::json!("ps_p0003_u2_text.png")
    );

    // Detection: one document per part, blocks routed by area and mapped.
    let blocks_of = |target: &str| -> Value {
        let write = plan
            .json_writes
            .iter()
            .find(|w| w.target == target)
            .unwrap_or_else(|| panic!("{target} written"));
        serde_json::from_str(&write.content).expect("valid json")
    };
    let first = blocks_of("ch1/text_detection/00001_blocks.json");
    assert_eq!(first["source_size"], serde_json::json!([50, 200]));
    assert_eq!(first["mask_size"], serde_json::json!([50, 200]));
    assert_eq!(first["mask_file"], serde_json::json!("00001_mask.png"));
    assert_eq!(first["blocks"].as_array().expect("blocks").len(), 1);
    assert_eq!(first["blocks"][0]["y2"], serde_json::json!(40.0));
    let second = blocks_of("ch1/text_detection/00002_blocks.json");
    assert_eq!(second["blocks"].as_array().expect("blocks").len(), 1);
    // Absolute page px 300 is px 100 of the bottom part.
    assert_eq!(second["blocks"][0]["y1"], serde_json::json!(100.0));
    assert_eq!(second["mask_file"], serde_json::json!("00002_mask.png"));
}

#[test]
fn build_plan_split_discards_a_deleted_part_and_trashes_only_its_artifacts() {
    let snapshot = snapshot_for_split();
    // Cut page 1 (50x400) in half and DELETE the bottom part: the split
    // degenerates into a crop, so no page index moves.
    let plan = build_plan(
        &snapshot,
        &split_op_deleted(1, SplitAxis::Horizontal, &[200], &[0, 1], &[false, true]),
        79,
    )
    .expect("plan builds");
    assert_eq!(plan.old_to_new, vec![Some(0), Some(1), Some(2), Some(3)]);
    assert_eq!(plan.new_page_count, 4);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("1 of 2 part(s) deleted")),
        "deleting user content must be reported: {:?}",
        plan.warnings
    );

    // Only the surviving part is staged, in every page-sized category.
    let staged = |target: &str| plan.creates.iter().any(|c| c.target == target);
    for kept in [
        "ch1/src/001.png",
        "ch1/clean_layers/001.png",
        "ch1/text_images/mask_page_1.png",
        "ch1/text_detection/00001_mask.png",
    ] {
        assert!(staged(kept), "{kept} must be staged");
    }
    for dropped in [
        "ch1/src/002.png",
        "ch1/clean_layers/002.png",
        "ch1/text_images/mask_page_2.png",
        "ch1/text_detection/00002_mask.png",
    ] {
        assert!(!staged(dropped), "{dropped} belongs to the deleted part");
    }
    assert!(
        !plan
            .json_writes
            .iter()
            .any(|w| w.target == "ch1/text_detection/00002_blocks.json"),
        "the deleted part gets no detection document"
    );

    // The deleted part's layer PNGs go to the TRASH, never onto a
    // surviving part's prefix; the survivor's PNG stays where it is.
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Trash { .. } => Some(m.from.as_str()),
            MoveDest::Final { .. } | MoveDest::Discard => None,
        })
        .collect();
    assert!(trashed.contains(&"ch1/layers/ps_p0001_bottom.png"));
    assert!(trashed.contains(&"ch1/layers/ps_p0001_text.png"));
    assert!(!trashed.contains(&"ch1/layers/ps_p0001_top.png"));
    assert!(
        !plan.moves.iter().any(|m| matches!(
            &m.dest,
            MoveDest::Final { path } if path.contains("ps_p0002_")
        )),
        "nothing of the deleted part may be renamed onto another page"
    );

    // The manifest keeps ONE entry: the deleted part's records are gone.
    let manifest = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/layers/layers.json")
        .expect("manifest rewritten");
    let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
    let pages = manifest["pages"].as_array().expect("pages");
    let cut_page: Vec<&Value> = pages
        .iter()
        .filter(|page| page["img_idx"] == serde_json::json!(1))
        .collect();
    assert_eq!(cut_page.len(), 1, "one entry for the one surviving part");
    let tree = cut_page[0]["tree"].as_array().expect("tree");
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0]["uid"], serde_json::json!("top"));

    // The deleted part's layer RECORDS are archived, not merely dropped:
    // their PNGs are recoverable from the trash, but the transform, group
    // membership and a text node's typed text live only in the manifest.
    let write = plan
        .trash_writes
        .iter()
        .find(|w| w.target.ends_with(DELETED_LAYERS_PAGES_FILE))
        .unwrap_or_else(|| {
            panic!("the deleted part's records must be archived: {:?}", plan.trash_writes)
        });
    let archived: Value = serde_json::from_str(&write.content).expect("valid json");
    let archived = archived.as_array().expect("array");
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0]["img_idx"], serde_json::json!(1));
    let uids: Vec<&str> = archived[0]["tree"]
        .as_array()
        .expect("tree")
        .iter()
        .filter_map(|rec| rec["uid"].as_str())
        .collect();
    assert_eq!(uids, vec!["text", "bottom"], "{archived:?}");
}

#[test]
fn build_plan_split_keeps_an_unroutable_layer_on_a_surviving_part() {
    let mut snapshot = snapshot_for_split();
    // A record the engine CANNOT route: no deform mesh, no transform. Its
    // PNG is referenced by nothing else.
    snapshot
        .committed
        .layers_files
        .insert("ps_p0001_placeless.png".to_string());
    let manifest = snapshot
        .committed
        .layers_manifest
        .as_mut()
        .expect("fixture manifest");
    manifest["pages"][0]["tree"]
        .as_array_mut()
        .expect("tree")
        .push(serde_json::json!({
            "uid": "placeless", "name": "P", "kind": "raster", "z": 3,
            "visible": true, "opacity": 1.0,
            "base_file": "ps_p0001_placeless.png"
        }));

    // Cut page 1 in half and delete the TOP part, so the old literal
    // "follows part 0" fallback would have destroyed the record.
    let plan = build_plan(
        &snapshot,
        &split_op_deleted(1, SplitAxis::Horizontal, &[200], &[0, 1], &[true, false]),
        80,
    )
    .expect("plan builds");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("'placeless'") && w.contains("first surviving part")),
        "the unroutable record must be reported: {:?}",
        plan.warnings
    );

    // It survives on the kept part, with its PNG left in place — a record
    // the geometry could not place is never deleted as a side effect.
    let manifest = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/layers/layers.json")
        .expect("manifest rewritten");
    let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
    let cut_page = manifest["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .find(|page| page["img_idx"] == serde_json::json!(1))
        .expect("the surviving part keeps the page index");
    assert!(
        cut_page["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .any(|rec| rec["uid"] == serde_json::json!("placeless")),
        "the unroutable record must survive: {cut_page}"
    );
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter_map(|m| match &m.dest {
            MoveDest::Trash { .. } => Some(m.from.as_str()),
            MoveDest::Final { .. } | MoveDest::Discard => None,
        })
        .collect();
    assert!(!trashed.contains(&"ch1/layers/ps_p0001_placeless.png"));
    // The genuinely deleted part's own PNG still goes to the trash.
    assert!(trashed.contains(&"ch1/layers/ps_p0001_top.png"));
}

#[test]
fn build_plan_split_falls_back_to_the_centre_when_a_text_render_cannot_be_probed() {
    let mut snapshot = snapshot_for_split();
    // The text node's PNG is unreadable, so its `image_size` stays unknown.
    snapshot.committed.layer_png_sizes.clear();
    let plan = build_plan(
        &snapshot,
        &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
        78,
    )
    .expect("plan builds");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("'text'") && w.contains("centre point")),
        "the degradation must be reported: {:?}",
        plan.warnings
    );
    // It still lands on the part its centre (cy 300) sits in, so the layer
    // is never lost — only the exactness of the rule degrades.
    let manifest = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/layers/layers.json")
        .expect("manifest rewritten");
    let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
    let bottom = manifest["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .find(|page| page["img_idx"] == serde_json::json!(2))
        .expect("bottom part present");
    assert!(
        bottom["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .any(|rec| rec["uid"] == serde_json::json!("text"))
    );
}

#[test]
fn build_plan_split_needs_page_sizes_and_warns_about_alt_versions() {
    let mut snapshot = snapshot_for_split();
    snapshot.has_alt_vers = true;
    let op = split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]);
    let plan = build_plan(&snapshot, &op, 79).expect("plan builds");
    assert!(
        plan.warnings.iter().any(|w| w.contains("alt_vers")),
        "changing the page count must warn: {:?}",
        plan.warnings
    );
    snapshot.page_sizes.clear();
    assert!(matches!(
        build_plan(&snapshot, &op, 80),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn build_plan_split_trashes_detection_it_cannot_cut() {
    let mut snapshot = snapshot_for_split();
    // A mask smaller than its page cannot be cut without inventing a scale.
    snapshot.detection[0].blocks = Some(DetectionBlocks::Parsed(serde_json::json!({
        "source_size": [50, 400],
        "mask_size": [25, 200],
        "blocks": [],
        "mask_file": "00001_mask.png"
    })));
    let plan = build_plan(
        &snapshot,
        &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
        81,
    )
    .expect("plan builds");
    assert!(
        plan.warnings.iter().any(|w| w.contains("text detection")),
        "the degradation must be reported: {:?}",
        plan.warnings
    );
    assert!(
        !plan
            .json_writes
            .iter()
            .any(|w| w.target.contains("text_detection")),
        "no detection document may be written"
    );
    assert!(
        !plan
            .creates
            .iter()
            .any(|c| c.target.contains("text_detection")),
        "no detection mask may be cut"
    );
}

// -----------------------------------------------------------------------
// Crop: one page rotated and cropped in place.
// -----------------------------------------------------------------------

/// A crop request over the shared fixture's page 1 (50x400).
fn crop_op(page_idx: usize, quarter_turns: u8, angle_deg: f64, rect: [u32; 4]) -> PageOpKind {
    PageOpKind::Crop {
        page_idx,
        quarter_turns,
        angle_deg,
        rect,
    }
}

/// The split fixture serves the crop unchanged: page 1 carries every
/// category a crop touches (clean overlay, typing mask, layers with a
/// probed text render, detection blocks + mask).
fn snapshot_for_crop() -> ChapterSnapshot {
    snapshot_for_split()
}

#[test]
fn crop_permutation_is_the_identity_and_rejects_bad_requests() {
    let perm = permutation_for_op(&crop_op(1, 1, 0.0, [0, 0, 200, 50]), 4).expect("valid");
    // The page COUNT does not change and no index moves.
    assert_eq!(
        map_of(&perm),
        vec![Some(0), Some(1), Some(2), Some(3)]
    );
    assert_eq!(perm.new_page_count, 4);
    assert!(perm.new_pages.is_empty());

    for bad in [
        // Page index out of range.
        crop_op(4, 0, 0.0, [0, 0, 10, 10]),
        // Quarter turns outside 0..=3.
        crop_op(1, 4, 0.0, [0, 0, 10, 10]),
        // Fine angle outside (-45, 45), and a non-finite one.
        crop_op(1, 0, 45.0, [0, 0, 10, 10]),
        crop_op(1, 0, -45.0, [0, 0, 10, 10]),
        crop_op(1, 0, f64::NAN, [0, 0, 10, 10]),
        // Zero-sized rectangles.
        crop_op(1, 0, 0.0, [0, 0, 0, 10]),
        crop_op(1, 0, 0.0, [0, 0, 10, 0]),
    ] {
        assert!(
            matches!(permutation_for_op(&bad, 4), Err(PageOpError::InvalidOp(_))),
            "must be refused: {bad:?}"
        );
    }
}

#[test]
fn crop_geometry_maps_every_coordinate_space_and_validates_the_rect() {
    // 100x200 page turned 90 CW -> a 200x100 canvas; keep its right half.
    let geo = CropGeometry::for_tests(1, [100, 200], 1, 0.0, [100, 0, 100, 100]);
    assert_eq!(geo.new_size(), [100, 100]);
    assert!(!geo.resamples());
    let map = geo.placement();
    // Page (0, 0) -> canvas (200, 0) -> new page (100, 0).
    assert_eq!(map.map_point(0.0, 0.0), (100.0, 0.0));
    // Page (100, 200) -> canvas (0, 100) -> new page (-100, 100): off the
    // new page's left edge, which a kept layer may legitimately be.
    assert_eq!(map.map_point(100.0, 200.0), (-100.0, 100.0));
    // A magnitude is not a coordinate: scale 1 leaves it alone.
    assert!((map.map_extent(7.5) - 7.5).abs() < f64::EPSILON);
    // The stored angle a page-placed object must gain.
    assert!((map.rotation_degrees() - 90.0).abs() < 1e-12);
    assert!((map.rotation_radians() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);

    // Survival: the page's top-left quadrant lands in the kept half.
    assert!(geo.keeps_point(10.0, 10.0));
    assert!(!geo.keeps_point(90.0, 190.0));
    assert!(geo.keeps_uv_point(0.1, 0.1));
    // A box straddling the frame edge survives; one entirely past it does not.
    assert!(geo.keeps_page_rect([40.0, 10.0, 60.0, 30.0]));
    assert!(!geo.keeps_page_rect([80.0, 150.0, 95.0, 190.0]));

    // A rectangle leaving the ROTATED canvas is refused, in the canvas'
    // own axes (200x100, not the page's 100x200).
    assert!(resolve_crop(1, [100, 200], 1, 0.0, [0, 0, 200, 100]).is_ok());
    assert!(resolve_crop(1, [100, 200], 1, 0.0, [0, 0, 100, 200]).is_err());
    assert!(resolve_crop(1, [100, 200], 1, 0.0, [1, 0, 200, 100]).is_err());
    assert!(resolve_crop(1, [0, 200], 1, 0.0, [0, 0, 10, 10]).is_err());
}

#[test]
fn crop_refuses_a_rotated_canvas_beyond_the_staging_budget() {
    // The rotated canvas becomes an RGBA staging buffer in the executor, so
    // it carries the same pixel budget as a stitched canvas — checked on the
    // CANVAS, which a rotation makes larger than the page it came from.
    // 7000x7000 = 49 MPx, comfortably inside the 200 MPx budget.
    assert!(u64::from(7_000u32) * u64::from(7_000u32) < STITCH_MAX_TOTAL_PX);
    assert!(resolve_crop(1, [7_000, 7_000], 0, 0.0, [0, 0, 10, 10]).is_ok());
    // 20000x20000 = 400 MPx, twice the budget.
    assert!(matches!(
        resolve_crop(1, [20_000, 20_000], 0, 0.0, [0, 0, 10, 10]),
        Err(PageOpError::InvalidOp(_))
    ));
    // A page that fits upright but whose ROTATED canvas does not: 45 deg
    // grows each side by ~sqrt(2), i.e. the area by 2.
    let snug = 12_000u32;
    assert!(resolve_crop(1, [snug, snug], 0, 0.0, [0, 0, 10, 10]).is_ok());
    let err = resolve_crop(1, [snug, snug], 0, 44.0, [0, 0, 10, 10])
        .expect_err("the rotated canvas exceeds the budget");
    assert!(
        format!("{err}").contains("exceeds"),
        "the message must name the budget: {err}"
    );
}

#[test]
fn build_plan_crop_rotates_every_raster_with_one_transform() {
    let snapshot = snapshot_for_crop();
    // Page 1 is 50x400; a 90 CW turn gives a 400x50 canvas, of which the
    // left half is kept.
    let plan = build_plan(&snapshot, &crop_op(1, 1, 0.0, [0, 0, 200, 50]), 90)
        .expect("plan builds");
    assert_eq!(
        plan.old_to_new,
        vec![Some(0), Some(1), Some(2), Some(3)],
        "a crop moves no page"
    );
    assert_eq!(plan.new_page_count, 4);

    // Every page-sized raster of the page is staged with the SAME page
    // size, the SAME rotation and the SAME crop, so they all come out
    // exactly the new page's size.
    let staged: Vec<(&str, u32, u32, &ComposeSource)> = plan
        .creates
        .iter()
        .filter_map(|create| match &create.content {
            NewPageContent::ComposedPng {
                width,
                height,
                sources,
                ..
            } => Some((create.target.as_str(), *width, *height, sources.first()?)),
            NewPageContent::CopyFile { .. } | NewPageContent::BlankPng { .. } => None,
        })
        .collect();
    let expect_raster = |target: &str, nearest: bool| {
        let (_, width, height, source) = staged
            .iter()
            .find(|(name, _, _, _)| *name == target)
            .unwrap_or_else(|| panic!("{target} must be staged: {staged:?}"));
        assert_eq!((*width, *height), (200, 50), "{target} size");
        assert_eq!(source.page_size, [50, 400], "{target} page size");
        assert_eq!(source.crop, [0, 0, 200, 50], "{target} crop");
        assert_eq!(source.dest, [0, 0, 200, 50], "{target} dest");
        let rotation = source.rotation.expect("a crop always records a rotation");
        assert_eq!(rotation.quarter_turns, 1, "{target} quarter turns");
        assert!(rotation.angle_deg == 0.0, "{target} angle");
        // The sampling policy belongs to the RASTER, so it governs the
        // page-size pre-resize as well as the rotation.
        assert_eq!(source.nearest, nearest, "{target} sampling");
    };
    expect_raster("ch1/src/001.png", false);
    expect_raster("ch1/clean_layers/001.png", false);
    // Masks are categorical and must not be interpolated.
    expect_raster("ch1/text_images/mask_page_1.png", true);
    expect_raster("ch1/text_detection/00001_mask.png", true);

    // The originals stay recoverable in the trash.
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter(|m| matches!(m.dest, MoveDest::Trash { .. }))
        .map(|m| m.from.as_str())
        .collect();
    for original in [
        "ch1/src/001.png",
        "ch1/clean_layers/001.png",
        "ch1/text_images/mask_page_1.png",
        "ch1/text_detection/00001_mask.png",
    ] {
        assert!(trashed.contains(&original), "{original} must be trashed");
    }
    // No other page's files move at all: the permutation is the identity.
    assert!(
        !plan.moves.iter().any(|m| m.from.contains("/000.")
            || m.from.contains("/002.")
            || m.from.contains("/003.")),
        "untouched pages must not move: {:?}",
        plan.moves
    );
}

#[test]
fn build_plan_crop_remaps_detection_under_a_quarter_turn() {
    let snapshot = snapshot_for_crop();
    let plan = build_plan(&snapshot, &crop_op(1, 1, 0.0, [0, 0, 200, 50]), 91)
        .expect("plan builds");
    let write = plan
        .json_writes
        .iter()
        .find(|w| w.target == "ch1/text_detection/00001_blocks.json")
        .expect("the detection document is REMAPPED, not trashed");
    let document: Value = serde_json::from_str(&write.content).expect("valid json");
    assert_eq!(document["source_size"], serde_json::json!([200, 50]));
    assert_eq!(document["mask_size"], serde_json::json!([200, 50]));
    assert_eq!(document["mask_file"], serde_json::json!("00001_mask.png"));
    let blocks = document["blocks"].as_array().expect("blocks");
    // The first block (y 2..40 of the page) rotates out of the kept half and
    // is dropped; the second (y 300..380) lands inside it.
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    let coord = |key: &str| blocks[0][key].as_f64().expect("number");
    assert!((coord("x1") - 20.0).abs() < 1e-9, "{blocks:?}");
    assert!((coord("y1") - 5.0).abs() < 1e-9, "{blocks:?}");
    assert!((coord("x2") - 100.0).abs() < 1e-9, "{blocks:?}");
    assert!((coord("y2") - 9.0).abs() < 1e-9, "{blocks:?}");
    assert!(
        plan.warnings.iter().any(|w| w.contains("entirely outside")),
        "the dropped block must be reported: {:?}",
        plan.warnings
    );
}

#[test]
fn build_plan_crop_trashes_detection_under_a_fine_angle() {
    let snapshot = snapshot_for_crop();
    // 50x400 at 5 deg -> an 85x403 canvas; keep a rectangle inside it.
    let plan = build_plan(&snapshot, &crop_op(1, 0, 5.0, [0, 0, 80, 400]), 92)
        .expect("plan builds");
    assert!(
        !plan
            .json_writes
            .iter()
            .any(|w| w.target.contains("text_detection")),
        "an axis-aligned block list cannot describe a freely rotated page"
    );
    assert!(
        !plan
            .creates
            .iter()
            .any(|c| c.target.contains("text_detection")),
        "no detection mask may be produced"
    );
    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter(|m| matches!(m.dest, MoveDest::Trash { .. }))
        .map(|m| m.from.as_str())
        .collect();
    assert!(trashed.contains(&"ch1/text_detection/00001_blocks.json"));
    assert!(trashed.contains(&"ch1/text_detection/00001_mask.png"));
    assert!(
        plan.warnings.iter().any(|w| w.contains("RESAMPLES")),
        "the fine-angle degradations must be named up front: {:?}",
        plan.warnings
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("text detection") && w.contains("trash")),
        "the detection degradation must be reported: {:?}",
        plan.warnings
    );
}

#[test]
fn build_plan_crop_archives_everything_the_frame_removed() {
    let mut snapshot = snapshot_for_crop();
    // One bubble inside the kept band, one below it.
    snapshot.committed.bubbles = Some(vec![
        serde_json::json!({"id": 1, "img_idx": 1, "img_u": 0.5, "img_v": 0.1,
                           "side": "left", "text": "in", "original_text": "in"}),
        serde_json::json!({"id": 2, "img_idx": 1, "img_u": 0.5, "img_v": 0.9,
                           "side": "left", "text": "out", "original_text": "out"}),
    ]);
    // Keep only the top 100 px of the 50x400 page, with no rotation: the
    // "top" layer (cy 50) survives, "text" (cy 300) and "bottom" (cy 350)
    // do not, and the position-less text_info entry reads as the page
    // centre (y 200), which is also gone.
    let plan = build_plan(&snapshot, &crop_op(1, 0, 0.0, [0, 0, 50, 100]), 95)
        .expect("plan builds");

    let trashed: Vec<&str> = plan
        .moves
        .iter()
        .filter(|m| matches!(m.dest, MoveDest::Trash { .. }))
        .map(|m| m.from.as_str())
        .collect();
    // A layer that survives nowhere takes its exclusively-claimed PNG with
    // it; a surviving layer's PNG is left exactly where it is.
    assert!(trashed.contains(&"ch1/layers/ps_p0001_text.png"), "{trashed:?}");
    assert!(trashed.contains(&"ch1/layers/ps_p0001_bottom.png"), "{trashed:?}");
    assert!(!trashed.contains(&"ch1/layers/ps_p0001_top.png"), "{trashed:?}");
    assert!(
        !plan
            .moves
            .iter()
            .any(|m| m.from == "ch1/layers/ps_p0001_top.png"),
        "a crop never renames a layer PNG: the page index does not move"
    );
    // The archived typing overlay's PNG follows its entry into the trash.
    assert!(
        trashed.contains(&"ch1/text_images/typing_overlay_p0001_1.png"),
        "{trashed:?}"
    );

    // Removed JSON entries are ARCHIVED, never silently dropped.
    let archived = |name: &str| -> Value {
        let write = plan
            .trash_writes
            .iter()
            .find(|w| w.target.ends_with(name))
            .unwrap_or_else(|| {
                panic!("{name} must be archived: {:?}", plan.trash_writes)
            });
        serde_json::from_str(&write.content).expect("valid json")
    };
    let bubbles = archived(DELETED_BUBBLES_FILE);
    let bubbles = bubbles.as_array().expect("array");
    assert_eq!(bubbles.len(), 1);
    assert_eq!(bubbles[0]["id"], serde_json::json!(2));
    assert_eq!(archived(DELETED_TEXT_INFO_FILE).as_array().expect("array").len(), 1);
    // A dropped LAYER record carries the layer's transform, its group
    // membership and — for a TEXT node — the typed text itself. Only its
    // rendered PNG goes to the trash, so without this archive that metadata
    // would be destroyed irrecoverably.
    let layers = archived(DELETED_LAYERS_PAGES_FILE);
    let layers = layers.as_array().expect("array");
    assert_eq!(layers.len(), 1, "one page contributed dropped records");
    // The page's ORIGINAL index, as for a page deleted whole.
    assert_eq!(layers[0]["img_idx"], serde_json::json!(1));
    let archived_tree = layers[0]["tree"].as_array().expect("tree");
    let uids: Vec<&str> = archived_tree
        .iter()
        .filter_map(|rec| rec["uid"].as_str())
        .collect();
    assert_eq!(uids, vec!["text", "bottom"], "{archived_tree:?}");
    // Records are archived VERBATIM, in the page's own coordinate space.
    assert_eq!(
        archived_tree[1]["transform"]["cy"],
        serde_json::json!(350.0)
    );
    // The group a dropped record belonged to travels with it.
    assert_eq!(layers[0]["groups"][0]["uid"], serde_json::json!("g1"));

    // The manifest keeps ONE page entry with only the surviving layer.
    let manifest: Value = serde_json::from_str(
        &plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/layers/layers.json")
            .expect("the manifest is rewritten")
            .content,
    )
    .expect("valid json");
    let page = manifest["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .find(|page| page["img_idx"] == serde_json::json!(1))
        .expect("page 1 still exists");
    let tree = page["tree"].as_array().expect("tree");
    assert_eq!(tree.len(), 1, "{tree:?}");
    assert_eq!(tree[0]["uid"], serde_json::json!("top"));
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("entirely outside the kept region")),
        "every drop must be reported: {:?}",
        plan.warnings
    );
}

#[test]
fn build_plan_crop_needs_page_sizes_and_warns_about_alt_versions() {
    let mut snapshot = snapshot_for_crop();
    snapshot.has_alt_vers = true;
    let op = crop_op(1, 1, 0.0, [0, 0, 200, 50]);
    let plan = build_plan(&snapshot, &op, 93).expect("plan builds");
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("alt_vers") && w.contains("OLD size")),
        "the page SIZE change must warn even though the count did not: {:?}",
        plan.warnings
    );
    snapshot.page_sizes.clear();
    assert!(matches!(
        build_plan(&snapshot, &op, 94),
        Err(PageOpError::InvalidOp(_))
    ));
}

#[test]
fn build_plan_identity_move_is_noop() {
    // A snapshot whose names are already canonical and an identity move.
    let mut snapshot = snapshot_for_plan();
    snapshot.detection.clear();
    let plan = build_plan(&snapshot, &PageOpKind::Move { from: 1, to: 1 }, 9)
        .expect("plan builds");
    assert!(plan.is_noop(), "identity op should plan no actions");
}
