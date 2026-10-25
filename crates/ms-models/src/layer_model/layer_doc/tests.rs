/*
File: models/layer_model/layer_doc/tests.rs

Purpose:
Unit tests of `layer_doc.rs`: `LayerDoc` page load / flush round trips, in-memory node and group
edits, text-node persistence, eviction, and the background-saver funnel (enqueue, coalescing,
autosave gate, drop). Scratch data lives under per-test `std::env::temp_dir()` paths.

Notes:
A child module of `layer_doc`, so it sees the parent's private items through `use super::*`.
*/

use super::*;
use eframe::egui::Color32;
use std::fs;

fn img(size: [usize; 2], c: Color32) -> ColorImage {
    ColorImage::filled(size, c)
}

fn tf(cx: f32, cy: f32, scale: f32) -> TransformRec {
    TransformRec {
        cx,
        cy,
        rotation: 0.0,
        scale,
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ld_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// A single-page page-size map for page 0, for `ensure_page_loaded` test calls.
fn psz(size: [usize; 2]) -> HashMap<usize, [usize; 2]> {
    let mut m = HashMap::new();
    m.insert(0, size);
    m
}

#[test]
fn ensure_page_loaded_reads_rasters_z_sorted() {
    let dir = temp_dir("load");

    // Seed two rasters on disk; add order is bottom-to-top, so r0 is below r1.
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "Bottom",
            visible: true,
            opacity: 1.0,
            transform: tf(10.0, 20.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r1",
            name: "Top",
            visible: false,
            opacity: 0.5,
            transform: tf(30.0, 40.0, 2.0),
            image: &img([3, 3], Color32::BLUE),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();

    let page = doc.page(0).expect("page resident");
    assert_eq!(page.nodes.len(), 2, "two raster nodes loaded");
    // Sorted bottom-to-top by z.
    assert!(page.nodes[0].z < page.nodes[1].z, "z sorted bottom-to-top");
    assert_eq!(page.nodes[0].uid, "r0");
    assert_eq!(page.nodes[1].uid, "r1");

    let r0 = doc.node(0, "r0").unwrap();
    assert!(r0.is_raster());
    assert!(r0.visible);
    assert!((r0.transform.cx - 10.0).abs() < 1e-6);
    assert!((r0.transform.cy - 20.0).abs() < 1e-6);

    let r1 = doc.node(0, "r1").unwrap();
    assert!(!r1.visible);
    assert!((r1.opacity - 0.5).abs() < 1e-6);
    assert!((r1.transform.scale - 2.0).abs() < 1e-6);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn page_has_pending_save_tracks_all_pending_persistence() {
    let dir = temp_dir("pending_save");
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r",
            name: "Raster",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    // A clean freshly-decoded page and a non-resident page have no pending save.
    assert!(
        !doc.page_has_pending_save(0),
        "freshly decoded page has no pending save"
    );
    assert!(
        !doc.page_has_pending_save(1),
        "non-resident page has no pending save"
    );

    // GEOMETRY-only edit: `set_transform` does NOT flag any node `pixels_dirty`; it is persisted
    // via a whole-page enqueue that reserves a `latest_epoch`. Driving that enqueue is the smallest
    // public API that exercises the `latest_epoch` gate (Gap-1 regression: the old pixel-only helper
    // classified this page "clean" and evicted it, losing the rotation).
    doc.enable_background_saver(None);
    doc.set_transform(0, "r", tf(5.0, 7.0, 1.0));
    doc.enqueue_page_save(0, &dir, None).unwrap();
    if let Some(handle) = doc.saver_handle() {
        // Let the job run so `shutdown_saver` joins cleanly; the ack is not polled, so
        // `latest_epoch` still holds the reserved epoch.
        let _ = handle.barrier_blocking();
    }
    assert!(
        doc.latest_epoch.contains_key(&(0, SaveKind::Raster))
            || doc.latest_epoch.contains_key(&(0, SaveKind::Text)),
        "the geometry enqueue reserved a latest_epoch"
    );
    assert!(
        !doc.pages.get(&0).unwrap().nodes.iter().any(|n| n.pixels_dirty),
        "a geometry edit does not flag any node pixels_dirty"
    );
    assert!(
        doc.page_has_pending_save(0),
        "a geometry edit awaiting its enqueued save is pending even with no pixels_dirty node"
    );

    // A PIXEL edit is likewise pending (via both `pixels_dirty` and the reserved epoch).
    doc.set_raster_pixels(
        0,
        "r",
        img([2, 2], Color32::BLUE),
        img([2, 2], Color32::BLUE),
        Vec::new(),
        true,
    );
    assert!(
        doc.page_has_pending_save(0),
        "pixel-edited resident page has a pending save"
    );

    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_transform_round_trips_through_flush() {
    let dir = temp_dir("tf");

    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r",
            name: "Pic",
            visible: true,
            opacity: 1.0,
            transform: tf(1.0, 1.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    doc.set_transform(0, "r", tf(123.0, 456.0, 3.5));
    // Geometry-only change must not bump the generation.
    assert_eq!(
        doc.node(0, "r").unwrap().generation,
        0,
        "set_transform does not bump generation"
    );
    doc.flush_page(0, &dir, None).unwrap();

    // A fresh doc reload sees the new transform, including scale.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let r = doc2.node(0, "r").unwrap();
    assert!((r.transform.cx - 123.0).abs() < 1e-6, "cx round-trips");
    assert!((r.transform.cy - 456.0).abs() < 1e-6, "cy round-trips");
    assert!((r.transform.scale - 3.5).abs() < 1e-6, "scale round-trips");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flush_page_dropping_raster_removes_it_from_disk() {
    // The typing tab's raster delete: remove the doc node, then flush DROPPING its uid so it does
    // not resurrect (save_page_rasters otherwise preserves a manifest raster as another tab's).
    let dir = temp_dir("drop_raster");
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "A",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r1",
            name: "B",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::BLUE),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert_eq!(doc.page(0).unwrap().nodes.len(), 2);

    // Remove r0 from the doc, then flush dropping it.
    assert!(doc.remove_node(0, "r0"));
    doc.flush_page_dropping_raster(0, &dir, None, "r0").unwrap();

    // A fresh reload sees only r1 (r0 gone, not resurrected); r1 intact.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert!(
        doc2.node(0, "r0").is_none(),
        "deleted raster did not resurrect on disk"
    );
    assert!(doc2.node(0, "r1").is_some(), "the other raster survives");
    assert_eq!(doc2.page(0).unwrap().nodes.len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_deform_bumps_version_and_round_trips_through_flush() {
    use super::super::manifest::DeformRec;
    let dir = temp_dir("dfm");

    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r",
            name: "Pic",
            visible: true,
            opacity: 1.0,
            transform: tf(1.0, 1.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert!(
        doc.node(0, "r").unwrap().deform.is_none(),
        "raster loads affine (no deform)"
    );

    let ver_before = doc.version();
    let gen_before = doc.node(0, "r").unwrap().generation;
    let mesh = DeformRec {
        cols: 2,
        rows: 2,
        points_px: vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]],
    };
    doc.set_deform(0, "r", Some(mesh.clone()));
    assert_eq!(
        doc.version(),
        ver_before + 1,
        "set_deform bumps the version"
    );
    assert_eq!(
        doc.node(0, "r").unwrap().generation,
        gen_before,
        "set_deform is geometry-only: no generation bump"
    );

    doc.flush_page(0, &dir, None).unwrap();

    // A fresh doc reload sees the persisted mesh.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let got = doc2
        .node(0, "r")
        .unwrap()
        .deform
        .as_ref()
        .expect("deform round-trips");
    assert_eq!(got.cols, 2);
    assert_eq!(got.rows, 2);
    assert_eq!(got.points_px, mesh.points_px);

    // Clearing the deform persists as None.
    doc2.set_deform(0, "r", None);
    doc2.flush_page(0, &dir, None).unwrap();
    let mut doc3 = LayerDoc::new();
    doc3.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert!(
        doc3.node(0, "r").unwrap().deform.is_none(),
        "cleared deform stays None"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_raster_mask_clip_bumps_generation_and_round_trips_through_flush() {
    let dir = temp_dir("rclip");
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r",
            name: "Pic",
            visible: true,
            opacity: 1.0,
            transform: tf(1.0, 1.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    // A freshly-loaded raster defaults OFF (no clip).
    if let NodeBody::Raster { mask_clip, .. } = &doc.node(0, "r").unwrap().body {
        assert_eq!(*mask_clip, None, "raster defaults to no mask-clip");
    } else {
        panic!("expected raster body");
    }

    let gen_before = doc.node(0, "r").unwrap().generation;
    doc.set_raster_mask_clip(0, "r", Some(true));
    assert!(
        doc.node(0, "r").unwrap().generation > gen_before,
        "set_raster_mask_clip bumps generation (so the tab re-clips + re-uploads)"
    );
    if let NodeBody::Raster { mask_clip, .. } = &doc.node(0, "r").unwrap().body {
        assert_eq!(*mask_clip, Some(true));
    } else {
        panic!("expected raster body");
    }

    doc.flush_page(0, &dir, None).unwrap();
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    if let NodeBody::Raster { mask_clip, .. } = &doc2.node(0, "r").unwrap().body {
        assert_eq!(
            *mask_clip,
            Some(true),
            "mask_clip round-trips through the doc + flush"
        );
    } else {
        panic!("expected raster body");
    }

    // set_raster_mask_clip is a no-op on a text node.
    doc2.add_node(0, text_node("t", [2, 2], Color32::WHITE));
    let t_gen = doc2.node(0, "t").unwrap().generation;
    doc2.set_raster_mask_clip(0, "t", Some(true));
    assert_eq!(
        doc2.node(0, "t").unwrap().generation,
        t_gen,
        "no-op on a text node"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_effects_bumps_generation_and_persists() {
    let dir = temp_dir("fx");

    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r",
            name: "Pic",
            visible: true,
            opacity: 1.0,
            transform: tf(1.0, 1.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let gen_before = doc.node(0, "r").unwrap().generation;

    let effects = vec![serde_json::json!({"type": "shadow"})];
    // A distinct (larger, different-colored) display image.
    doc.set_effects(0, "r", effects, img([4, 4], Color32::BLUE));

    let node = doc.node(0, "r").unwrap();
    assert!(node.generation > gen_before, "set_effects bumps generation");
    // display_image now differs from the base.
    if let NodeBody::Raster {
        base_image,
        display_image,
        ..
    } = &node.body
    {
        assert_eq!(base_image.size, [2, 2]);
        assert_eq!(display_image.size, [4, 4], "display image replaced");
        assert_ne!(
            base_image.size, display_image.size,
            "display distinct from base"
        );
    } else {
        panic!("expected raster body");
    }

    doc.flush_page(0, &dir, None).unwrap();

    // Reload: effects present, display distinct from base.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let r = doc2.node(0, "r").unwrap();
    if let NodeBody::Raster {
        base_image,
        display_image,
        effects,
        ..
    } = &r.body
    {
        assert_eq!(effects.len(), 1, "effects chain persisted");
        assert_eq!(display_image.size, [4, 4], "rendered display persisted");
        assert_eq!(base_image.size, [2, 2], "base preserved (non-destructive)");
        assert_ne!(
            base_image.size, display_image.size,
            "display still distinct from base"
        );
    } else {
        panic!("expected raster body");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reorder_node_one_swaps_z() {
    let dir = temp_dir("reorder");

    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "A",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r1",
            name: "B",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::BLUE),
        },
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();

    let order_before: Vec<String> = doc
        .page(0)
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.uid.clone())
        .collect();
    assert_eq!(order_before, vec!["r0".to_string(), "r1".to_string()]);

    // Move bottom node (r0) up one: it swaps z with r1, so order flips.
    let changed = doc.reorder_node_one(0, "r0", true);
    assert!(changed, "reorder reports a change");

    let order_after: Vec<String> = doc
        .page(0)
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.uid.clone())
        .collect();
    assert_eq!(
        order_after,
        vec!["r1".to_string(), "r0".to_string()],
        "order flipped"
    );

    // Already at the top: moving up again is a no-op.
    assert!(
        !doc.reorder_node_one(0, "r0", true),
        "top node can't move up"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reorder_group_block_moves_whole_group_past_neighbour() {
    // Order (bottom-to-top): a (g0), b (g0), c (ungrouped). Moving g0 up should hop past c.
    let mut doc = doc_with_empty_page();
    let mut a = raster_node("a", [2, 2], Color32::RED);
    a.group_uid = Some("g0".into());
    let mut b = raster_node("b", [2, 2], Color32::GREEN);
    b.group_uid = Some("g0".into());
    let c = raster_node("c", [2, 2], Color32::BLUE);
    doc.add_node(0, a);
    doc.add_node(0, b);
    doc.add_node(0, c);
    doc.add_group(
        0,
        GroupMeta {
            uid: "g0".into(),
            name: "G".into(),
            visible: true,
            opacity: 1.0,
            collapsed: false,
        },
    );

    let order = |d: &LayerDoc| -> Vec<String> {
        d.page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect()
    };
    assert_eq!(order(&doc), vec!["a", "b", "c"]);

    // Move the g0 block up: it hops past the ungrouped c, preserving intra-group order a<b.
    assert!(doc.reorder_group_block(0, "g0", true), "group block moved");
    assert_eq!(
        order(&doc),
        vec!["c", "a", "b"],
        "group hopped past c, intra-order kept"
    );
    // z stays contiguous and sorted.
    let zs: Vec<u32> = doc.page(0).unwrap().nodes.iter().map(|n| n.z).collect();
    assert_eq!(zs, vec![0, 1, 2], "z reassigned contiguously");

    // Move it back down: g0 hops past c again, restoring the original order.
    assert!(
        doc.reorder_group_block(0, "g0", false),
        "group block moved back"
    );
    assert_eq!(
        order(&doc),
        vec!["a", "b", "c"],
        "moved back to original order"
    );

    // g0 is now the bottom block; moving it down further is a no-op.
    let before = doc.version();
    assert!(
        !doc.reorder_group_block(0, "g0", false),
        "g0 already at the bottom"
    );
    assert_eq!(doc.version(), before, "no-op did not bump version");

    // Unknown group → false.
    assert!(!doc.reorder_group_block(0, "nope", true));
}

#[test]
fn set_z_order_reassigns_by_uid_order() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
    doc.add_node(0, raster_node("b", [2, 2], Color32::GREEN));
    doc.add_node(0, raster_node("c", [2, 2], Color32::BLUE));

    let order = |d: &LayerDoc| -> Vec<String> {
        d.page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect()
    };
    assert_eq!(order(&doc), vec!["a", "b", "c"]);

    // Reverse the order via an explicit uid list.
    doc.set_z_order(0, &["c".into(), "b".into(), "a".into()]);
    assert_eq!(
        order(&doc),
        vec!["c", "b", "a"],
        "reordered to the listed order"
    );
    let zs: Vec<u32> = doc.page(0).unwrap().nodes.iter().map(|n| n.z).collect();
    assert_eq!(zs, vec![0, 1, 2], "z = position in the list");

    // A partial list places listed nodes first (in list order), unlisted after in prior order.
    doc.set_z_order(0, &["b".into()]);
    assert_eq!(order(&doc)[0], "b", "listed node placed first");
}

fn save_png(path: &std::path::Path, w: u32, h: u32, c: [u8; 4]) {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba(c));
    img.save(path).unwrap();
}

/// Appends a legacy REFERENCE-only text node (payload in `text_info.json` via `payload_ref`, no
/// inline `render_data`) onto page 0 of an existing `layers.json`, for the legacy-load tests. The
/// production text writer (`write_page_text_payload`) always writes the inline v3 payload, so it
/// can't produce a reference-only node; this mutates the manifest JSON directly.
fn add_legacy_text_ref_node(
    dir: &std::path::Path,
    uid: &str,
    z: u32,
    layer_idx: u32,
    pinned: bool,
    visible: bool,
    opacity: f32,
) {
    let path = dir.join("layers.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let pages = manifest["pages"].as_array_mut().unwrap();
    let page = pages
        .iter_mut()
        .find(|p| p["img_idx"] == 0)
        .expect("page 0 exists");
    page["tree"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "uid": uid, "name": uid, "kind": "text", "z": z,
            "layer_idx": layer_idx, "pinned": pinned,
            "visible": visible, "opacity": opacity,
            "payload_ref": { "store": "text_info", "uid": uid }
        }));
    fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
}

/// Adds a `text_groups` band entry (layer_idx → z) to page 0's manifest.
fn add_text_group_band(dir: &std::path::Path, layer_idx: u32, z: u32) {
    let path = dir.join("layers.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let page = manifest["pages"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["img_idx"] == 0)
        .expect("page 0 exists");
    page["text_groups"]
        .as_array_mut()
        .map(|a| a.push(serde_json::json!({ "layer_idx": layer_idx, "z": z, "name": "g" })))
        .unwrap_or_else(|| {
            page["text_groups"] =
                serde_json::json!([{ "layer_idx": layer_idx, "z": z, "name": "g" }]);
        });
    fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
}

#[test]
fn degenerate_equal_band_z_two_groups_keep_layer_idx_then_y_order() {
    // ITEM F (insurance): a degenerate manifest where TWO text groups share the SAME band Z. The
    // re-rank's `layer_idx` tiebreak (before page-Y) must reproduce the old `(layer_idx, page-Y)`
    // sub-order exactly: group 0's members (by Y) then group 1's members (by Y).
    let dir = temp_dir("degenerate_two_groups");
    // A raster first, to create the page manifest (and sit at the bottom, z=0).
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "R",
            visible: true,
            opacity: 1.0,
            transform: tf(50.0, 50.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    // Both groups at the SAME band Z = 5 (degenerate; normally distinct).
    add_text_group_band(&dir, 0, 5);
    add_text_group_band(&dir, 1, 5);
    // group 0: a (Y=30), b (Y=10); group 1: c (Y=20), d (Y=5). All unpinned at node z=0.
    for uid in ["a", "b", "c", "d"] {
        add_legacy_text_ref_node(
            &dir,
            uid,
            0,
            if matches!(uid, "a" | "b") { 0 } else { 1 },
            false,
            true,
            1.0,
        );
    }
    let mk = |uid: &str, layer_idx: u32, y: f32| {
        save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
        serde_json::json!({
            "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
            "layer_idx": layer_idx, "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
            "render_data": { "text": uid }
        })
    };
    let overlays = serde_json::json!([
        mk("a", 0, 30.0),
        mk("b", 0, 10.0),
        mk("c", 1, 20.0),
        mk("d", 1, 5.0)
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();
    let order: Vec<&str> = doc
        .page(0)
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.uid.as_str())
        .collect();
    // Raster at the bottom (z=0 < group z=5), then (layer_idx asc, then page-Y asc):
    // group0 = b(10), a(30); group1 = d(5), c(20).
    assert_eq!(
        order,
        vec!["r0", "b", "a", "d", "c"],
        "old (layer_idx, page-Y) sub-order reproduced"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_text_group_flattens_to_per_text_bands_preserving_visual_order() {
    // PART 1 (order preservation, the main risk): a legacy chapter with a raster + a TEXT GROUP of
    // 3 unpinned texts (sub-ordered by page-Y) must, after the on-read flatten, keep the SAME visual
    // order (group members ascending page-Y, interleaved with the raster by the group's band Z) while
    // giving every text its OWN unique Z (individually movable). No reshuffle.
    let dir = temp_dir("legacy_group_flatten");

    // Raster r0 at the BOTTOM (add_page_raster assigns z above existing bands → 0 here).
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "R",
            visible: true,
            opacity: 1.0,
            transform: tf(50.0, 50.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    // Text group 0 band ABOVE the raster (z = 5). Three unpinned members.
    add_text_group_band(&dir, 0, 5);
    for uid in ["tc", "ta", "tb"] {
        add_legacy_text_ref_node(&dir, uid, 0, 0, false, true, 1.0);
    }
    // text_info.json: members at distinct page-Y (ta=10 top, tb=20 mid, tc=30 bottom). The group
    // sub-order is ASCENDING page-Y, so the in-group bottom-to-top order is ta, tb, tc.
    let mk = |uid: &str, y: f32| {
        save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
        serde_json::json!({
            "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
            "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
            "render_data": { "text": uid }
        })
    };
    let overlays = serde_json::json!([mk("ta", 10.0), mk("tb", 20.0), mk("tc", 30.0)]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();
    let page = doc.page(0).expect("resident");

    // Bottom-to-top: raster (group band was z=5 > raster z=0), then the group members by page-Y.
    let order: Vec<&str> = page.nodes.iter().map(|n| n.uid.as_str()).collect();
    assert_eq!(
        order,
        vec!["r0", "ta", "tb", "tc"],
        "visual order preserved, group flattened by page-Y"
    );
    // Every node now has a UNIQUE, contiguous Z (each text individually movable).
    let zs: Vec<u32> = page.nodes.iter().map(|n| n.z).collect();
    assert_eq!(zs, vec![0, 1, 2, 3], "each band has a unique sequential Z");

    // IDEMPOTENCY: flush to disk → reload → order unchanged (no drift across save/reload).
    doc.flush_page(0, &dir, None).unwrap();
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();
    let order2: Vec<String> = doc2
        .page(0)
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.uid.clone())
        .collect();
    assert_eq!(
        order2,
        vec!["r0", "ta", "tb", "tc"],
        "order stable across a save/reload cycle"
    );
    // On disk every text is now pinned-with-explicit-Z; no TextGroup band remains.
    let bands = persist::load_page_bands(&dir, None, 0);
    assert!(
        !bands.iter().any(|b| matches!(
            b,
            crate::layer_model::ordering::Band::TextGroup { .. }
        )),
        "groups dissolved into per-text pinned bands after the flush"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn new_text_node_is_added_on_top() {
    // PART 1(d): a NEW text overlay goes to the TOP of the unified Z (max Z + 1), not auto-by-Y.
    let dir = temp_dir("new_text_top");
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "R",
            visible: true,
            opacity: 1.0,
            transform: tf(50.0, 50.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    add_legacy_text_ref_node(&dir, "t0", 5, 0, true, true, 1.0);
    save_png(&dir.join("t0.png"), 4, 3, [0, 255, 0, 255]);
    let overlays = serde_json::json!([{
        "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
        "img_x_px": 5.0, "img_y_px": 5.0, "rotation_deg": 0.0, "scale": 1.0,
        "render_data": { "text": "t0" }
    }]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();

    // Add a NEW text node (a freshly-created overlay routes through `add_node`).
    let new_node = LayerNode {
        uid: "tnew".into(),
        name: "New".into(),
        kind: NodeKind::Text,
        z: 0, // set on top by add_node
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: Some(0),
        text_pinned: false,
        text_pinned_by_group: false,
        transform: TransformRec {
            cx: 5.0,
            cy: 1.0,
            rotation: 0.0,
            scale: 1.0,
        }, // very high on page (low Y)
        deform: None,
        generation: 0,
        pixels_dirty: false,
        body: NodeBody::Text {
            render_data: serde_json::json!({ "text": "new" }),
            image: img([2, 2], Color32::GREEN),
            is_image: false,
            payload_uid: "tnew".into(),
            mask_clip: None,
            extra_centers: RenderedTextExtraInfo::default(),
            centering_frame: None,
        },
    };
    assert!(doc.add_node(0, new_node));

    // Despite a low page-Y (which auto-Y would have sunk), the new text is on TOP.
    let order: Vec<String> = doc
        .page(0)
        .unwrap()
        .nodes
        .iter()
        .map(|n| n.uid.clone())
        .collect();
    assert_eq!(
        order.last().map(String::as_str),
        Some("tnew"),
        "new text is on top (max Z + 1)"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flattened_text_can_be_ordered_below_a_raster_and_back() {
    // A flattened text owns its own band, so it can move BELOW a raster (Part 1 unifies text+raster Z)
    // and back, via the doc Z order (the same set_z_order the ⬆/⬇ band move uses).
    let dir = temp_dir("text_below_raster");
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "R",
            visible: true,
            opacity: 1.0,
            transform: tf(50.0, 50.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    add_legacy_text_ref_node(&dir, "t0", 5, 0, true, true, 1.0);
    save_png(&dir.join("t0.png"), 4, 3, [0, 255, 0, 255]);
    let overlays = serde_json::json!([{
        "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
        "img_x_px": 5.0, "img_y_px": 5.0, "rotation_deg": 0.0, "scale": 1.0,
        "render_data": { "text": "t0" }
    }]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();
    // Initially text above raster.
    assert_eq!(
        doc.page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect::<Vec<_>>(),
        vec!["r0".to_string(), "t0".to_string()]
    );
    // Move text BELOW the raster.
    doc.set_z_order(0, &["t0".to_string(), "r0".to_string()]);
    assert_eq!(
        doc.page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect::<Vec<_>>(),
        vec!["t0".to_string(), "r0".to_string()],
        "text now below the raster"
    );
    // And back above.
    doc.set_z_order(0, &["r0".to_string(), "t0".to_string()]);
    assert_eq!(
        doc.page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect::<Vec<_>>(),
        vec!["r0".to_string(), "t0".to_string()]
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ensure_page_loaded_reads_text_node_interleaved_by_z() {
    let dir = temp_dir("text_load");

    // A raster at the bottom (z = 0). add_page_raster assigns Z above existing bands → 0 here.
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "Bottom",
            visible: true,
            opacity: 1.0,
            transform: tf(10.0, 20.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();

    // A pinned text node at z = 5 (above the raster), referencing an overlay payload by uid.
    add_legacy_text_ref_node(&dir, "t0", 5, 0, true, false, 0.75);

    // The text overlay PNG + a text_info.json entry keyed by the same uid.
    let png_name = "t0.png";
    save_png(&dir.join(png_name), 4, 3, [0, 255, 0, 255]);
    let overlays = serde_json::json!([
        {
            "uid": "t0",
            "img_idx": 0,
            "overlay_type": "text",
            "file": png_name,
            "img_x_px": 111.0,
            "img_y_px": 222.0,
            "rotation_deg": 0.0,
            "scale": 1.0,
            "render_data": { "text": "Hello", "size": 24 }
        }
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
        .unwrap();

    let page = doc.page(0).expect("page resident");
    assert_eq!(page.nodes.len(), 2, "raster + text loaded");
    // Interleaved bottom-to-top by z: raster (z=0) below text (z=5).
    assert_eq!(page.nodes[0].uid, "r0");
    assert!(page.nodes[0].is_raster());
    assert_eq!(page.nodes[1].uid, "t0");
    assert!(page.nodes[1].is_text());
    assert!(
        page.nodes[0].z < page.nodes[1].z,
        "text sorts above the raster"
    );

    let t = doc.node(0, "t0").unwrap();
    assert!(t.is_text());
    assert!(!t.visible, "visible carried from the text node");
    assert!(
        (t.opacity - 0.75).abs() < 1e-6,
        "opacity carried from the text node"
    );
    // Transform decoded from the overlay payload (img_x/y → center, deg→rad).
    assert!((t.transform.cx - 111.0).abs() < 1e-6);
    assert!((t.transform.cy - 222.0).abs() < 1e-6);
    assert!((t.transform.rotation - 0.0).abs() < 1e-6);
    assert!((t.transform.scale - 1.0).abs() < 1e-6);
    assert_eq!(
        t.display_image().size,
        [4, 3],
        "rendered overlay PNG loaded"
    );
    if let NodeBody::Text {
        render_data,
        payload_uid,
        ..
    } = &t.body
    {
        assert_eq!(payload_uid, "t0");
        assert_eq!(render_data["text"], "Hello");
    } else {
        panic!("expected text body");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ensure_page_loaded_loads_overlays_without_layers_json_nodes() {
    // Regression: most overlays live ONLY in text_info.json — a layers.json text NODE exists only
    // for PS pin/group overrides. The doc must load EVERY page overlay from text_info.json, not
    // just the sparse nodes, otherwise PS (which projects its text layers from the doc) shows
    // almost none ("doesn't display text layers, except sometimes one").
    let dir = temp_dir("text_no_node");
    fs::create_dir_all(&dir).unwrap();

    // Two overlays for page 0 + one for page 1, all in text_info.json, with NO layers.json nodes.
    save_png(&dir.join("a.png"), 4, 3, [0, 255, 0, 255]);
    save_png(&dir.join("b.png"), 5, 2, [0, 0, 255, 255]);
    let overlays = serde_json::json!([
        { "uid": "a", "img_idx": 0, "overlay_type": "text", "file": "a.png",
          "img_x_px": 10.0, "img_y_px": 20.0, "rotation_deg": 0.0, "scale": 1.0,
          "render_data": { "text": "A" } },
        { "uid": "b", "img_idx": 0, "overlay_type": "image", "file": "b.png",
          "img_x_px": 30.0, "img_y_px": 40.0, "rotation_deg": 0.0, "scale": 1.0,
          "render_data": {} },
        { "uid": "other", "img_idx": 1, "overlay_type": "text", "file": "a.png" }
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let page = doc.page(0).expect("page resident");
    assert_eq!(
        page.nodes.len(),
        2,
        "both page-0 overlays loaded despite having no layers.json nodes"
    );
    assert!(
        doc.node(0, "a").is_some_and(|n| n.is_text() && n.visible),
        "default visible=true"
    );
    assert!(
        doc.node(0, "b")
            .is_some_and(|node| { matches!(node.body, NodeBody::Text { is_image: true, .. }) }),
        "legacy image overlay retains its image marker"
    );
    assert!(
        doc.node(0, "other").is_none(),
        "page-1 overlay not loaded onto page 0"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn decode_reads_legacy_text_images_dir_and_mints_stable_uid() {
    // A never-migrated legacy chapter keeps its overlays ONLY under `text_images/` (a MIDDLE-format
    // `text_info.json` with img_idx/img_x_px/render_data but NO uid) and has no `layers/layers.json`.
    // `decode_page_payload` must read that dir (when threaded as `legacy_text_dir`) and mint a
    // deterministic uid for the uid-less entry, so the PS editor's doc-fed text is non-empty.
    let root = temp_dir("legacy_text_images");
    let primary = root.join("layers");
    let legacy = root.join("text_images");
    fs::create_dir_all(&primary).unwrap();
    fs::create_dir_all(&legacy).unwrap();

    // MIDDLE-format overlay + its PNG live ONLY in `text_images/` — nothing in the layers dirs.
    let file_name = "typing_overlay_p0000_1700000000.png";
    save_png(&legacy.join(file_name), 7, 4, [255, 0, 0, 255]);
    let overlays = serde_json::json!([
        { "img_idx": 0, "overlay_type": "text", "file": file_name,
          "img_x_px": 12.0, "img_y_px": 34.0, "rotation_deg": 0.0, "scale": 1.0,
          "render_data": { "text": "Legacy" } }
    ]);
    fs::write(
        legacy.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let expected_uid = text_payload::stable_overlay_uid(file_name);

    // WITH the legacy dir: the overlay resolves to a Text node keyed by the deterministic uid.
    let payload = LayerDoc::decode_page_payload(0, &primary, None, Some(&legacy), &psz([100, 100]))
        .expect("decode succeeds with legacy dir");
    assert_eq!(
        payload.nodes.len(),
        1,
        "the legacy text_images overlay loads when the legacy dir is threaded through"
    );
    let node = &payload.nodes[0];
    assert_eq!(node.uid, expected_uid, "uid-less legacy entry gets the stable uid");
    assert!(node.is_text(), "loaded as a text node");
    assert_eq!(node.display_image().size, [7, 4], "legacy PNG resolved from text_images/");

    // WITHOUT the legacy dir (gate closed / migrated chapter): NO text node is produced.
    let payload_none = LayerDoc::decode_page_payload(0, &primary, None, None, &psz([100, 100]))
        .expect("decode succeeds without legacy dir");
    assert!(
        payload_none.nodes.is_empty(),
        "no legacy text source ⇒ no text node (proves the gate/threading matters)"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ensure_page_loaded_skips_text_node_without_overlay() {
    let dir = temp_dir("text_no_overlay");

    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "Bottom",
            visible: true,
            opacity: 1.0,
            transform: tf(0.0, 0.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    // A text node whose payload has no matching text_info.json entry → skipped (warned).
    add_legacy_text_ref_node(&dir, "ghost", 3, 0, true, true, 1.0);

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let page = doc.page(0).unwrap();
    assert_eq!(
        page.nodes.len(),
        1,
        "text node without an overlay is skipped"
    );
    assert!(page.nodes[0].is_raster());

    let _ = fs::remove_dir_all(&dir);
}

/// Builds a text `LayerNode` carrying render_data + transform + deform + mask_clip, for flush tests.
fn text_node_with_payload(uid: &str) -> LayerNode {
    LayerNode {
        uid: uid.into(),
        name: "Текст".into(),
        kind: NodeKind::Text,
        z: 0,
        visible: false,
        opacity: 0.5,
        group_uid: None,
        text_layer_idx: Some(0),
        text_pinned: false,
        text_pinned_by_group: false,
        transform: TransformRec {
            cx: 111.0,
            cy: 222.0,
            rotation: 0.7,
            scale: 1.5,
        },
        deform: Some(DeformRec {
            cols: 2,
            rows: 2,
            points_px: vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]],
        }),
        generation: 0,
        pixels_dirty: true, // dirty → flush writes the rendered PNG
        body: NodeBody::Text {
            render_data: serde_json::json!({"text": "Hello", "size": 24}),
            image: img([4, 3], Color32::GREEN),
            is_image: false,
            payload_uid: uid.into(),
            mask_clip: Some(true),
            extra_centers: RenderedTextExtraInfo::default(),
            centering_frame: None,
        },
    }
}

#[test]
fn text_payload_round_trips_through_flush() {
    // A text node with full inline payload (render_data + transform + deform + mask_clip) flushes
    // into layers.json (no text_info.json) and reloads byte-for-byte identical.
    let dir = temp_dir("text_flush_rt");

    let mut doc = doc_with_empty_page();
    doc.add_node(0, text_node_with_payload("t0"));
    doc.flush_page(0, &dir, None).unwrap();
    // No text_info.json was written: the inline payload is self-sufficient.
    assert!(
        !dir.join("text_info.json").exists(),
        "flush writes no text_info.json"
    );

    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc2
        .node(0, "t0")
        .expect("inline text node reloads from layers.json alone");
    assert!(t.is_text());
    assert!(!t.visible, "visible round-trips");
    assert!((t.opacity - 0.5).abs() < 1e-6, "opacity round-trips");
    assert!((t.transform.cx - 111.0).abs() < 1e-4, "cx round-trips");
    assert!((t.transform.cy - 222.0).abs() < 1e-4, "cy round-trips");
    assert!(
        (t.transform.rotation - 0.7).abs() < 1e-5,
        "rotation (radians) round-trips"
    );
    assert!((t.transform.scale - 1.5).abs() < 1e-6, "scale round-trips");
    let d = t.deform.as_ref().expect("deform round-trips");
    assert_eq!(
        d.points_px,
        vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]]
    );
    assert_eq!(t.display_image().size, [4, 3], "rendered PNG round-trips");
    if let NodeBody::Text {
        render_data,
        mask_clip,
        payload_uid,
        ..
    } = &t.body
    {
        assert_eq!(render_data["text"], "Hello");
        assert_eq!(render_data["size"], 24);
        assert_eq!(*mask_clip, Some(true), "mask_clip persisted");
        assert_eq!(payload_uid, "t0");
    } else {
        panic!("expected text body");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn non_finite_centers_are_treated_as_not_measured() {
    // `serde_json` writes a non-finite float as a bare `null` without erroring, and the next
    // `read_manifest` then fails to decode that `null` into an `f32` — which makes the chapter's
    // whole `layers.json` unreadable AND unwritable. So a non-finite center must never reach the
    // record; it is dropped exactly like an unmeasured one.
    let only_bad_mean = text_centers_rec_from(&RenderedTextExtraInfo {
        mean_center: Some([f32::NAN, 1.0]),
        median_center: None,
    });
    assert_eq!(
        only_bad_mean, None,
        "a NaN component makes the only center unusable ⇒ no record at all"
    );

    let mixed = text_centers_rec_from(&RenderedTextExtraInfo {
        mean_center: Some([1.0, f32::INFINITY]),
        median_center: Some([3.0, 4.0]),
    })
    .expect("the finite median still yields a record");
    assert_eq!(mixed.mean, None, "the infinite mean was dropped");
    assert_eq!(
        mixed.median,
        Some([3.0, 4.0]),
        "the finite median was kept untouched"
    );

    let good = text_centers_rec_from(&RenderedTextExtraInfo {
        mean_center: Some([-2.5, 0.0]),
        median_center: Some([-2.0, 0.5]),
    })
    .expect("all-finite centers are recorded");
    assert_eq!(good.mean, Some([-2.5, 0.0]));
    assert_eq!(good.median, Some([-2.0, 0.5]));
}

#[test]
fn centering_state_survives_a_flush_reload_cycle() {
    // The schema-v4 centering-assist state (renderer centers + guide frame) must survive a doc
    // flush and a fresh load — that is the whole point of persisting it: closing and reopening a
    // chapter must not collapse the bound center onto the plain image center.
    let dir = temp_dir("centering_flush_rt");

    let mut doc = doc_with_empty_page();
    let mut node = text_node_with_payload("t0");
    if let NodeBody::Text {
        extra_centers,
        centering_frame,
        ..
    } = &mut node.body
    {
        extra_centers.mean_center = Some([2.5, 1.5]);
        extra_centers.median_center = Some([2.0, 1.0]);
        *centering_frame = Some(CenteringFrameRec {
            cx: 111.0,
            cy: 222.0,
            half_w: 30.0,
            half_h: 20.0,
        });
    } else {
        panic!("expected text body");
    }
    doc.add_node(0, node);
    doc.flush_page(0, &dir, None).unwrap();

    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc2.node(0, "t0").expect("text node reloads");
    let NodeBody::Text {
        extra_centers,
        centering_frame,
        ..
    } = &t.body
    else {
        panic!("expected text body");
    };
    assert_eq!(
        extra_centers.mean_center,
        Some([2.5, 1.5]),
        "mean center survives the reload"
    );
    assert_eq!(
        extra_centers.median_center,
        Some([2.0, 1.0]),
        "median center survives the reload"
    );
    let frame = centering_frame.expect("guide frame survives the reload");
    assert!((frame.cx - 111.0).abs() < 1e-4);
    assert!((frame.cy - 222.0).abs() < 1e-4);
    assert!((frame.half_w - 30.0).abs() < 1e-4);
    assert!((frame.half_h - 20.0).abs() < 1e-4);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_node_that_measured_no_centers_reloads_as_never_measured() {
    // The absent case: an all-`None` measurement must not be written, and must reload as
    // all-`None` rather than as some empty-but-present record.
    let dir = temp_dir("centering_flush_absent");

    let mut doc = doc_with_empty_page();
    doc.add_node(0, text_node_with_payload("t0"));
    doc.flush_page(0, &dir, None).unwrap();

    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc2.node(0, "t0").expect("text node reloads");
    let NodeBody::Text {
        extra_centers,
        centering_frame,
        ..
    } = &t.body
    else {
        panic!("expected text body");
    };
    assert_eq!(extra_centers, &RenderedTextExtraInfo::default());
    assert!(centering_frame.is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn text_flush_preserves_existing_raster_and_vice_versa() {
    // Flushing text payload must not drop a raster node on the same page, and a later raster flush
    // must not drop the text node.
    let dir = temp_dir("text_flush_preserve");

    // Seed a raster on disk, load it, add a text node, flush.
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "Pic",
            visible: true,
            opacity: 1.0,
            transform: tf(5.0, 5.0, 1.0),
            image: &img([2, 2], Color32::RED),
        },
    )
    .unwrap();
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    doc.add_node(0, text_node_with_payload("t0"));
    doc.flush_page(0, &dir, None).unwrap();

    // Both kinds present on a fresh reload.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert!(
        doc2.node(0, "r0").is_some_and(LayerNode::is_raster),
        "raster preserved across text flush"
    );
    assert!(
        doc2.node(0, "t0").is_some_and(LayerNode::is_text),
        "text node persisted"
    );

    // The raster is still loadable from persist (kind-filter preserved it).
    let rasters = persist::load_page_rasters(&dir, None, 0).unwrap();
    assert_eq!(rasters.layers.len(), 1, "raster survives in layers.json");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_v2_text_node_migrates_on_read() {
    // A v2 layers.json (text node with payload_ref, NO inline render_data) plus a text_info.json
    // entry must load into a populated NodeBody::Text (migration-on-read).
    let dir = temp_dir("text_migrate_v2");
    fs::create_dir_all(&dir).unwrap();

    // v2-style manifest: a text node referencing text_info.json by payload_ref, no inline payload.
    let manifest = serde_json::json!({
        "schema_version": 2,
        "pages": [{
            "img_idx": 0,
            "tree": [{
                "uid": "t0", "name": "T", "kind": "text", "z": 3,
                "layer_idx": 0, "pinned": true,
                "visible": true, "opacity": 0.8,
                "payload_ref": { "store": "text_info", "uid": "t0" }
            }]
        }]
    });
    fs::write(
        dir.join("layers.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();

    // The legacy payload + PNG live in text_info.json.
    save_png(&dir.join("t0.png"), 6, 5, [0, 255, 0, 255]);
    let overlays = serde_json::json!([
        { "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
          "img_x_px": 50.0, "img_y_px": 60.0, "rotation_deg": 90.0, "scale": 2.0,
          "render_data": { "text": "Legacy" } }
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc.node(0, "t0").expect("legacy text node loaded");
    assert!(t.is_text());
    assert!(
        (t.opacity - 0.8).abs() < 1e-6,
        "opacity from the layers.json override"
    );
    // Geometry decoded from the legacy entry (deg→rad).
    assert!((t.transform.cx - 50.0).abs() < 1e-4);
    assert!(
        (t.transform.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
        "90° → π/2"
    );
    assert!((t.transform.scale - 2.0).abs() < 1e-6);
    assert_eq!(t.display_image().size, [6, 5], "legacy PNG loaded");
    if let NodeBody::Text { render_data, .. } = &t.body {
        assert_eq!(render_data["text"], "Legacy");
    } else {
        panic!("expected text body");
    }

    // Migration: flush writes the page inline; a fresh reload reads from the INLINE payload (name +
    // mask_clip preserved) and does NOT need text_info.json.
    doc.flush_page(0, &dir, None).unwrap();
    // Remove the legacy file: the migrated page must load without it.
    let _ = fs::remove_file(dir.join("text_info.json"));
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc2
        .node(0, "t0")
        .expect("migrated text node reloads from inline alone");
    assert_eq!(
        t.name, "T",
        "persisted node name round-trips (not regenerated)"
    );
    assert!(
        (t.transform.scale - 2.0).abs() < 1e-6,
        "geometry migrated to inline"
    );
    if let NodeBody::Text { render_data, .. } = &t.body {
        assert_eq!(
            render_data["text"], "Legacy",
            "render_data migrated to inline"
        );
    } else {
        panic!("expected text body");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn typing_style_edit_flush_reload_preserves_geometry_and_render_data() {
    // Mirrors the typing tab's flow: add a text node, edit geometry + render via the doc mutators,
    // flush text, reload — geometry + render_data + grouping must round-trip from layers.json alone.
    let dir = temp_dir("typing_flow");

    let mut doc = doc_with_empty_page();
    let mut node = text_node_with_payload("ov1");
    node.text_layer_idx = Some(3);
    doc.add_node(0, node);

    // Geometry edit (drag) + a re-render (text change) routed through the doc mutators.
    doc.set_transform(
        0,
        "ov1",
        TransformRec {
            cx: 77.0,
            cy: 88.0,
            rotation: 0.3,
            scale: 2.5,
        },
    );
    doc.set_text_render(
        0,
        "ov1",
        serde_json::json!({"text": "Edited", "size": 40}),
        img([6, 5], Color32::BLUE),
        RenderedTextExtraInfo::default(),
    );
    doc.flush_page_text(0, &dir, None).unwrap();

    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    let t = doc2.node(0, "ov1").expect("text node reloads");
    assert!(
        (t.transform.cx - 77.0).abs() < 1e-4,
        "edited cx round-trips"
    );
    assert!(
        (t.transform.scale - 2.5).abs() < 1e-6,
        "edited scale round-trips"
    );
    assert!(
        (t.transform.rotation - 0.3).abs() < 1e-5,
        "edited rotation round-trips"
    );
    assert_eq!(t.text_layer_idx, Some(3), "text-group axis round-trips");
    assert_eq!(
        t.display_image().size,
        [6, 5],
        "re-rendered image round-trips"
    );
    if let NodeBody::Text { render_data, .. } = &t.body {
        assert_eq!(render_data["text"], "Edited");
        assert_eq!(render_data["size"], 40);
    } else {
        panic!("expected text body");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_text_render_marks_dirty_and_flush_reencodes_png() {
    // F2: set_text_render must set pixels_dirty so the next flush re-encodes the rendered PNG with
    // the new pixels (otherwise a re-rendered text keeps a stale PNG on disk).
    let dir = temp_dir("text_reencode");

    let mut doc = doc_with_empty_page();
    doc.add_node(0, text_node_with_payload("t0"));
    // First flush writes the initial PNG (RED-ish from text_node_with_payload: GREEN actually).
    doc.flush_page_text(0, &dir, None).unwrap();
    let png_path = dir.join(persist::text_image_file_name(0, "t0"));
    let before = std::fs::read(&png_path).expect("initial png written");

    // Re-render with a DIFFERENT-sized, different-colored image; set_text_render sets pixels_dirty.
    doc.set_text_render(
        0,
        "t0",
        serde_json::json!({"text": "New"}),
        img([9, 7], Color32::from_rgb(1, 2, 3)),
        RenderedTextExtraInfo::default(),
    );
    assert!(
        doc.node(0, "t0").unwrap().pixels_dirty,
        "set_text_render marks pixels_dirty"
    );
    doc.flush_page_text(0, &dir, None).unwrap();
    let after = std::fs::read(&png_path).expect("png still present");
    assert_ne!(before, after, "flush re-encoded the changed text PNG");

    // Reload sees the new pixels.
    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert_eq!(
        doc2.node(0, "t0").unwrap().display_image().size,
        [9, 7],
        "new render size persisted"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn migrated_page_ignores_stale_legacy_text_info() {
    // After a page is migrated (inline node present), a DELETED overlay still lingering in the
    // legacy text_info.json must NOT resurrect on load (the migration gate ignores the legacy file).
    let dir = temp_dir("migrate_gate");

    let mut doc = doc_with_empty_page();
    doc.add_node(0, text_node_with_payload("keep"));
    doc.flush_page_text(0, &dir, None).unwrap();

    // Plant a stale legacy entry for a DIFFERENT uid that is NOT in the inline set.
    save_png(&dir.join("ghost.png"), 3, 3, [255, 0, 0, 255]);
    let overlays = serde_json::json!([
        { "uid": "ghost", "img_idx": 0, "overlay_type": "text", "file": "ghost.png",
          "img_x_px": 1.0, "img_y_px": 1.0, "rotation_deg": 0.0, "scale": 1.0, "render_data": {} }
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let mut doc2 = LayerDoc::new();
    doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
        .unwrap();
    assert!(doc2.node(0, "keep").is_some(), "inline node loads");
    assert!(
        doc2.node(0, "ghost").is_none(),
        "stale legacy overlay does NOT resurrect on a migrated page"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ensure_page_loaded_ribbon_uses_full_chapter_page_sizes() {
    // Regression (reviewer HIGH): loading a ribbon-format page through the doc must pass the FULL
    // chapter page-size map so the cross-page ribbon scale is solved against real aspects. With
    // non-uniform aspects (page0 100×250, page1 100×300), a single-page map would corrupt page-1
    // geometry. The doc result must equal the shared `migrate_overlay_entries`(full map) reference.
    let dir = temp_dir("ribbon_doc");
    fs::create_dir_all(&dir).unwrap();

    // Two ribbon overlays: one on page 0 (constrains the scale), one on page 1 (under test).
    save_png(&dir.join("a.png"), 4, 3, [0, 255, 0, 255]);
    save_png(&dir.join("b.png"), 4, 3, [0, 0, 255, 255]);
    let overlays = serde_json::json!([
        {"uid":"a","page":"1_1","x":10.0,"y":10.0,"region_w":20.0,"region_h":4.0,
         "file":"a.png","render_data":{"text":"A"}},
        {"uid":"b","page":"1_2","x":10.0,"y":150.0,"region_w":20.0,"region_h":4.0,
         "file":"b.png","render_data":{"text":"B"}},
    ]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    let full: HashMap<usize, [usize; 2]> =
        [(0, [100, 250]), (1, [100, 300])].into_iter().collect();

    // Reference: the shared codec's full-map migration + per-entry decode for the page-1 entry.
    let raw = text_payload::read_overlay_entries(&[dir.as_path()]);
    let migrated = text_payload::migrate_overlay_entries(&raw, &full, |_| (0.0, 0.0));
    let ref_obj = migrated
        .iter()
        .find_map(|e| {
            e.as_object()
                .filter(|o| o.get("uid").and_then(Value::as_str) == Some("b"))
        })
        .expect("page-1 entry migrated")
        .clone();
    let ref_placement = text_payload::decode_overlay_placement(&ref_obj, [100, 300]);

    // The doc loads page 1 with the FULL map and must produce the same transform.
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(1, &dir, None, None, &full).unwrap();
    let t = doc.node(1, "b").expect("page-1 ribbon overlay loaded");
    assert!(
        (t.transform.cx - ref_placement.transform.cx).abs() < 1e-3,
        "cx matches full-map reference: doc={} ref={}",
        t.transform.cx,
        ref_placement.transform.cx
    );
    assert!(
        (t.transform.cy - ref_placement.transform.cy).abs() < 1e-3,
        "cy matches full-map reference: doc={} ref={}",
        t.transform.cy,
        ref_placement.transform.cy
    );
    // Sanity: a single-page map would have produced a different cy (the corruption case).
    let single: HashMap<usize, [usize; 2]> = [(1, [100, 300])].into_iter().collect();
    let migrated_bad = text_payload::migrate_overlay_entries(&raw, &single, |_| (0.0, 0.0));
    let bad_obj = migrated_bad
        .iter()
        .find_map(|e| {
            e.as_object()
                .filter(|o| o.get("uid").and_then(Value::as_str) == Some("b"))
        })
        .unwrap()
        .clone();
    let bad_cy = text_payload::decode_overlay_placement(&bad_obj, [100, 300])
        .transform
        .cy;
    assert!(
        (t.transform.cy - bad_cy).abs() > 1e-2,
        "the full-map doc result differs from the buggy single-page result (corruption avoided)"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn text_node_body_helpers() {
    // Construct a Text node directly (disk loading is step 1b) and exercise the Text variant.
    let node = LayerNode {
        uid: "t".into(),
        name: "Hello".into(),
        kind: NodeKind::Text,
        z: 0,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: Some(0),
        text_pinned: false,
        text_pinned_by_group: false,
        transform: tf(0.0, 0.0, 1.0),
        deform: None,
        generation: 0,
        pixels_dirty: false,
        body: NodeBody::Text {
            render_data: serde_json::json!({"text": "Hello", "size": 24}),
            image: img([5, 6], Color32::WHITE),
            is_image: false,
            payload_uid: "payload-123".into(),
            mask_clip: None,
            extra_centers: RenderedTextExtraInfo::default(),
            centering_frame: None,
        },
    };

    assert!(node.is_text());
    assert!(!node.is_raster());
    assert_eq!(
        node.display_image().size,
        [5, 6],
        "display_image returns the text image"
    );
    if let NodeBody::Text {
        payload_uid,
        render_data,
        ..
    } = &node.body
    {
        assert_eq!(payload_uid, "payload-123");
        assert_eq!(render_data["text"], "Hello");
    } else {
        panic!("expected text body");
    }
}

/// Builds a raster `LayerNode` in memory (no disk), for the in-memory mutator tests.
fn raster_node(uid: &str, size: [usize; 2], c: Color32) -> LayerNode {
    LayerNode {
        uid: uid.into(),
        name: uid.into(),
        kind: NodeKind::Raster,
        z: 0,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: None,
        text_pinned: false,
        text_pinned_by_group: false,
        transform: tf(0.0, 0.0, 1.0),
        deform: None,
        generation: 0,
        pixels_dirty: false,
        body: NodeBody::Raster {
            base_image: img(size, c),
            display_image: img(size, c),
            effects: Vec::new(),
            base_file: format!("{uid}.png"),
            mask_clip: None,
        },
    }
}

/// Builds a text `LayerNode` in memory (no disk), for the in-memory mutator tests.
fn text_node(uid: &str, size: [usize; 2], c: Color32) -> LayerNode {
    LayerNode {
        uid: uid.into(),
        name: uid.into(),
        kind: NodeKind::Text,
        z: 0,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: Some(0),
        text_pinned: false,
        text_pinned_by_group: false,
        transform: tf(0.0, 0.0, 1.0),
        deform: None,
        generation: 0,
        pixels_dirty: false,
        body: NodeBody::Text {
            render_data: serde_json::json!({"text": "x"}),
            image: img(size, c),
            is_image: false,
            payload_uid: uid.into(),
            mask_clip: None,
            extra_centers: RenderedTextExtraInfo::default(),
            centering_frame: None,
        },
    }
}

/// A resident, empty page for in-memory mutator tests (no disk involved).
fn doc_with_empty_page() -> LayerDoc {
    let mut doc = LayerDoc::new();
    doc.pages.insert(
        0,
        DocPage {
            nodes: Vec::new(),
            groups: Vec::new(),
        },
    );
    doc
}

#[test]
fn mutations_bump_the_document_version() {
    let mut doc = doc_with_empty_page();
    let mut last = doc.version();

    // Each mutator that changes something must advance the version.
    let mut assert_bumped = |doc: &LayerDoc, what: &str| {
        assert!(
            doc.version() > last,
            "{what} must bump the version (was {last}, now {})",
            doc.version()
        );
        assert_eq!(doc.page_revision(0), doc.version(), "{what} must stamp page 0 with the new version");
        last = doc.version();
    };

    assert!(doc.add_node(0, raster_node("a", [2, 2], Color32::RED)));
    assert_bumped(&doc, "add_node");
    assert!(doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE)));
    assert_bumped(&doc, "add_node b");

    doc.set_transform(0, "a", tf(5.0, 5.0, 2.0));
    assert_bumped(&doc, "set_transform");
    doc.set_visibility(0, "a", false);
    assert_bumped(&doc, "set_visibility");
    doc.set_opacity(0, "a", 0.5);
    assert_bumped(&doc, "set_opacity");
    doc.set_effects(
        0,
        "a",
        vec![serde_json::json!({"x": 1})],
        img([3, 3], Color32::GREEN),
    );
    assert_bumped(&doc, "set_effects");
    doc.set_raster_pixels(
        0,
        "a",
        img([4, 4], Color32::RED),
        img([4, 4], Color32::RED),
        Vec::new(),
        true,
    );
    assert_bumped(&doc, "set_raster_pixels");
    assert!(doc.reorder_node_one(0, "a", true));
    assert_bumped(&doc, "reorder_node_one");

    let group = GroupMeta {
        uid: "g0".into(),
        name: "G".into(),
        visible: true,
        opacity: 1.0,
        collapsed: false,
    };
    doc.add_group(0, group);
    assert_bumped(&doc, "add_group");
    doc.set_group(0, "a", Some("g0".into()));
    assert_bumped(&doc, "set_group");
    doc.remove_group(0, "g0");
    assert_bumped(&doc, "remove_group");

    doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
    assert_bumped(&doc, "add_node t");
    doc.set_text_render(
        0,
        "t",
        serde_json::json!({"text": "y"}),
        img([2, 2], Color32::WHITE),
        RenderedTextExtraInfo::default(),
    );
    assert_bumped(&doc, "set_text_render");

    assert!(doc.remove_node(0, "a"));
    assert_bumped(&doc, "remove_node");

    // A no-op (missing node) must NOT bump the version.
    let before_noop = doc.version();
    doc.set_visibility(0, "missing", true);
    assert_eq!(
        doc.version(),
        before_noop,
        "no-op must not bump the version"
    );

    // Evicting a resident page bumps; evicting an absent page does not.
    doc.evict_page(0);
    assert!(doc.version() > before_noop, "evict_page bumps the version");
    let after_evict = doc.version();
    doc.evict_page(0);
    assert_eq!(
        doc.version(),
        after_evict,
        "evicting an absent page is a no-op"
    );
}

/// Hashes every content field of a resident page (`None` when the page is not resident), for the
/// page-revision property test. It destructures `DocPage`, `LayerNode`, `NodeBody`, `GroupMeta`
/// and `ColorImage` EXHAUSTIVELY (no `..`), so a new field fails to compile here until it is
/// classified as content (hashed) or bookkeeping (named and ignored, with the reason).
fn page_content_hash(page: Option<&DocPage>) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    fn hash_image(h: &mut DefaultHasher, image: &ColorImage) {
        let ColorImage { size, source_size, pixels } = image;
        size.hash(h);
        source_size.x.to_bits().hash(h);
        source_size.y.to_bits().hash(h);
        pixels.hash(h);
    }

    let DocPage { nodes, groups } = page?;
    let mut h = DefaultHasher::new();
    for node in nodes {
        let LayerNode {
            uid,
            name,
            kind,
            z,
            visible,
            opacity,
            group_uid,
            text_layer_idx,
            text_pinned,
            text_pinned_by_group,
            transform,
            deform,
            // Bookkeeping, not content: `generation` is the GPU-cache counter that content
            // mutators advance alongside the page revision, and `pixels_dirty` is persistence
            // state that saves and acks flip without changing anything visible.
            generation: _,
            pixels_dirty: _,
            body,
        } = node;
        uid.hash(&mut h);
        name.hash(&mut h);
        matches!(kind, NodeKind::Raster).hash(&mut h);
        z.hash(&mut h);
        visible.hash(&mut h);
        opacity.to_bits().hash(&mut h);
        group_uid.hash(&mut h);
        text_layer_idx.hash(&mut h);
        text_pinned.hash(&mut h);
        text_pinned_by_group.hash(&mut h);
        // `Debug` of these records prints every field (f32 in round-trip form).
        format!("{transform:?}|{deform:?}").hash(&mut h);
        match body {
            NodeBody::Raster { base_image, display_image, effects, base_file, mask_clip } => {
                0_u8.hash(&mut h);
                hash_image(&mut h, base_image);
                hash_image(&mut h, display_image);
                serde_json::Value::Array(effects.clone()).to_string().hash(&mut h);
                base_file.hash(&mut h);
                mask_clip.hash(&mut h);
            }
            NodeBody::Text { render_data, image, is_image, payload_uid, mask_clip, extra_centers, centering_frame } => {
                1_u8.hash(&mut h);
                render_data.to_string().hash(&mut h);
                hash_image(&mut h, image);
                is_image.hash(&mut h);
                payload_uid.hash(&mut h);
                mask_clip.hash(&mut h);
                format!("{extra_centers:?}|{centering_frame:?}").hash(&mut h);
            }
        }
    }
    for group in groups {
        let GroupMeta { uid, name, visible, opacity, collapsed } = group;
        uid.hash(&mut h);
        name.hash(&mut h);
        visible.hash(&mut h);
        opacity.to_bits().hash(&mut h);
        collapsed.hash(&mut h);
    }
    Some(h.finish())
}

/// A fresh three-node, one-group page payload for the property test.
fn property_page_payload() -> DecodedPagePayload {
    let mut r1 = raster_node("r1", [2, 2], Color32::BLUE);
    r1.z = 1;
    r1.group_uid = Some("g0".into());
    let mut t0 = text_node("t0", [2, 2], Color32::WHITE);
    t0.z = 2;
    DecodedPagePayload {
        nodes: vec![raster_node("r0", [2, 2], Color32::RED), r1, t0],
        groups: vec![GroupMeta { uid: "g0".into(), name: "G".into(), visible: true, opacity: 1.0, collapsed: false }],
    }
}

/// Number of operations in the property test's table (`apply_property_op`).
const PROPERTY_OPS: usize = 24;

/// Applies operation `op` (0..PROPERTY_OPS) to `page` with arguments drawn from `pick` (a value in
/// a small range, so unchanged values, absent uids and absent pages occur). Every `LayerDoc`
/// mutator has a step here; a new mutator must get one too.
fn apply_property_op(doc: &mut LayerDoc, op: usize, page: usize, pick: &mut impl FnMut(usize) -> usize) {
    const UIDS: [&str; 5] = ["r0", "r1", "t0", "n0", "missing"];
    const GROUPS: [&str; 3] = ["g0", "g1", "gone"];
    let uid = UIDS[pick(UIDS.len())];
    let group = GROUPS[pick(GROUPS.len())];
    let color = [Color32::RED, Color32::GREEN][pick(2)];
    let flag = pick(2) == 1;
    let node_count = doc.page(page).map_or(0, |p| p.nodes.len());
    match op {
        0 => doc.insert_decoded_page(page, property_page_payload()),
        1 => doc.evict_page(page),
        2 => doc.set_transform(page, uid, tf(if flag { 1.0 } else { 0.0 }, 0.0, 1.0)),
        3 => doc.set_deform(page, uid, flag.then(|| DeformRec { cols: 1, rows: 1, points_px: vec![[0.0, 0.0]; 4] })),
        4 => doc.set_raster_mask_clip(page, uid, [None, Some(true), Some(false)][pick(3)]),
        5 => doc.set_effects(page, uid, if flag { vec![serde_json::json!({"x": 1})] } else { Vec::new() }, img([2, 2], color)),
        6 if node_count < 6 => {
            doc.add_node(page, raster_node("n0", [2, 2], color));
        }
        7 if node_count < 6 => {
            doc.add_node_at_z(page, text_node("n0", [2, 2], color), 10 + u32::from(flag));
        }
        6 | 7 => {}
        8 => {
            doc.remove_node(page, uid);
        }
        9 => doc.set_visibility(page, uid, flag),
        10 => doc.set_opacity(page, uid, [0.5, 1.0, 1.5][pick(3)]),
        11 => doc.set_group(page, uid, flag.then(|| group.to_owned())),
        12 => {
            doc.set_text_pin_meta(page, uid, flag, pick(2) == 1);
        }
        13 => doc.add_group(page, GroupMeta { uid: group.into(), name: "G".into(), visible: true, opacity: 1.0, collapsed: false }),
        14 => {
            doc.set_group_meta(page, group, flag, [0.5, 1.0, f32::NAN][pick(3)]);
        }
        15 => {
            doc.set_group_collapsed(page, group, flag);
        }
        16 => doc.remove_group(page, group),
        17 => doc.set_raster_pixels(page, uid, img([2, 2], color), img([2, 2], color), Vec::new(), flag),
        18 => doc.set_text_render(page, uid, serde_json::json!({"text": if flag { "a" } else { "b" }}), img([2, 2], color), RenderedTextExtraInfo::default()),
        19 => {
            doc.reorder_node_one(page, uid, flag);
        }
        20 => {
            doc.reorder_group_block(page, group, flag);
        }
        21 => {
            let mut order: Vec<String> = UIDS.iter().map(|u| (*u).to_owned()).collect();
            if flag {
                order.reverse();
            }
            doc.set_z_order(page, &order);
        }
        22 => {
            // A raw `node_mut` edit through a funnel: the caller marks the page itself.
            if let Some(node) = doc.node_mut(page, uid) {
                node.name = if flag { "renamed".into() } else { node.uid.clone() };
                doc.mark_page_changed(page);
            }
        }
        23 => {
            // Bookkeeping that must not count as content: re-dirtying rasters after a failed ack.
            doc.mark_page_rasters_dirty(page);
        }
        _ => unreachable!("op {op} is outside the PROPERTY_OPS table"),
    }
}

/// S-050 contract over a seeded random walk of every mutator (incl. no-ops): a content change of a
/// page always changes that page's revision (I1), never another page's revision or content (I4),
/// the document version advances iff some page stamp did (I2), and stamps only grow and never
/// exceed the version (I3), across evict and re-insert.
#[test]
fn page_revision_tracks_every_content_change() {
    const PAGES: usize = 3;
    let mut doc = LayerDoc::new();
    for page in 0..PAGES {
        doc.insert_decoded_page(page, property_page_payload());
    }
    // Seeded LCG (Knuth MMIX constants): deterministic, no dependency.
    let mut state: u64 = 0x5EED_0050;
    let mut pick = move |n: usize| -> usize {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let high = usize::try_from(state >> 33).expect("a 31-bit value fits in usize");
        high % n
    };
    let mut content_changes = [0_usize; PROPERTY_OPS];
    for step in 0..4000 {
        let op = pick(PROPERTY_OPS);
        let target = pick(PAGES);
        let before: Vec<(Option<u64>, u64)> =
            (0..PAGES).map(|p| (page_content_hash(doc.page(p)), doc.page_revision(p))).collect();
        let version_before = doc.version();
        apply_property_op(&mut doc, op, target, &mut pick);
        let mut any_stamp_moved = false;
        for (page, (hash_before, rev_before)) in before.into_iter().enumerate() {
            let hash_after = page_content_hash(doc.page(page));
            let rev_after = doc.page_revision(page);
            let ctx = format!("step {step} op {op} target {target} page {page}");
            assert!(rev_after >= rev_before, "{ctx}: a page revision never decreases");
            assert!(rev_after <= doc.version(), "{ctx}: a page revision never exceeds the version");
            if hash_after != hash_before {
                assert_ne!(rev_after, rev_before, "{ctx}: a content change must advance the page revision");
                content_changes[op] += 1;
            }
            if page != target {
                assert_eq!(hash_after, hash_before, "{ctx}: an op on another page changed this page");
                assert_eq!(rev_after, rev_before, "{ctx}: an op on another page moved this page's revision");
            }
            any_stamp_moved |= rev_after != rev_before;
        }
        assert_eq!(doc.version() != version_before, any_stamp_moved, "step {step} op {op}: the version advances iff a page stamp did");
    }
    // Every content mutator must really have changed content at least once, or its step in the
    // table is vacuous; op 23 is the bookkeeping-only control.
    for (op, count) in content_changes.iter().enumerate().take(PROPERTY_OPS - 1) {
        assert!(*count > 0, "op {op} never changed page content in the walk");
    }
    assert_eq!(content_changes[PROPERTY_OPS - 1], 0, "bookkeeping changed page content");
}

#[test]
fn add_node_puts_new_node_on_top_and_keeps_sorted() {
    let mut doc = doc_with_empty_page();

    // First node into an empty page → z = 0.
    assert!(doc.add_node(0, raster_node("a", [2, 2], Color32::RED)));
    assert_eq!(doc.node(0, "a").unwrap().z, 0);

    // Second node → one above the current max.
    assert!(doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE)));
    assert_eq!(doc.node(0, "b").unwrap().z, 1, "new node on top");

    let page = doc.page(0).unwrap();
    assert_eq!(page.nodes.len(), 2);
    // Sorted bottom-to-top by z.
    assert_eq!(page.nodes[0].uid, "a");
    assert_eq!(page.nodes[1].uid, "b");
    assert!(page.nodes[0].z < page.nodes[1].z, "kept sorted by z");

    // Absent page → false.
    assert!(!doc.add_node(99, raster_node("c", [2, 2], Color32::GREEN)));
}

#[test]
fn remove_node_removes_by_uid_and_reports_missing() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
    doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE));

    assert!(doc.remove_node(0, "a"), "existing node removed");
    assert!(doc.node(0, "a").is_none());
    assert_eq!(doc.page(0).unwrap().nodes.len(), 1);

    assert!(!doc.remove_node(0, "missing"), "missing uid returns false");
    assert!(!doc.remove_node(99, "a"), "absent page returns false");
}

#[test]
fn set_visibility_and_opacity_clamps() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, raster_node("a", [2, 2], Color32::RED));

    doc.set_visibility(0, "a", false);
    assert!(!doc.node(0, "a").unwrap().visible, "visibility toggled off");
    doc.set_visibility(0, "a", true);
    assert!(doc.node(0, "a").unwrap().visible, "visibility toggled on");

    doc.set_opacity(0, "a", 0.25);
    assert!((doc.node(0, "a").unwrap().opacity - 0.25).abs() < 1e-6);
    doc.set_opacity(0, "a", 5.0);
    assert!(
        (doc.node(0, "a").unwrap().opacity - 1.0).abs() < 1e-6,
        "opacity clamped high"
    );
    doc.set_opacity(0, "a", -3.0);
    assert!(
        (doc.node(0, "a").unwrap().opacity - 0.0).abs() < 1e-6,
        "opacity clamped low"
    );
}

#[test]
fn set_group_meta_updates_visible_and_opacity_and_bumps_version_only_on_change() {
    let mut doc = doc_with_empty_page();
    doc.add_group(
        0,
        GroupMeta {
            uid: "g0".into(),
            name: "Group".into(),
            visible: true,
            opacity: 1.0,
            collapsed: true,
        },
    );
    let v0 = doc.version();
    assert!(doc.set_group_meta(0, "g0", false, 0.5));
    let g = &doc.page(0).unwrap().groups[0];
    assert!(!g.visible && (g.opacity - 0.5).abs() < 1e-6);
    assert!(g.collapsed, "the panel-only collapse flag is untouched");
    let v1 = doc.version();
    assert!(v1 > v0, "a change bumps the version");

    assert!(doc.set_group_meta(0, "g0", false, 0.5), "an existing group reports true");
    assert_eq!(doc.version(), v1, "an unchanged value does not bump the version");

    assert!(doc.set_group_meta(0, "g0", false, 7.0));
    assert!((doc.page(0).unwrap().groups[0].opacity - 1.0).abs() < 1e-6, "opacity clamped high");

    let v2 = doc.version();
    assert!(!doc.set_group_meta(0, "missing", true, 1.0), "unknown group uid");
    assert!(!doc.set_group_meta(9, "g0", true, 1.0), "absent page");
    assert!(!doc.set_group_meta(0, "g0", true, f32::NAN), "non-finite opacity refused");
    assert_eq!(doc.version(), v2, "refused calls change nothing");
    assert!(!doc.page(0).unwrap().groups[0].visible);
}

#[test]
fn set_group_collapsed_sets_only_the_flag_and_bumps_only_on_change() {
    let mut doc = doc_with_empty_page();
    doc.add_group(
        0,
        GroupMeta {
            uid: "g0".into(),
            name: "Group".into(),
            visible: false,
            opacity: 0.5,
            collapsed: false,
        },
    );
    let v0 = doc.version();
    assert!(doc.set_group_collapsed(0, "g0", true));
    let g = &doc.page(0).unwrap().groups[0];
    assert!(g.collapsed);
    assert!(!g.visible && (g.opacity - 0.5).abs() < 1e-6, "composite meta untouched");
    let v1 = doc.version();
    assert!(v1 > v0, "a change bumps the version");
    assert!(doc.set_group_collapsed(0, "g0", true));
    assert_eq!(doc.version(), v1, "an unchanged flag does not bump");
    assert!(!doc.set_group_collapsed(0, "missing", false), "unknown group uid");
    assert!(!doc.set_group_collapsed(4, "g0", false), "absent page");
    assert_eq!(doc.version(), v1);
}

/// The group meta reaches disk through the ordinary doc page save (sync flush AND the background
/// saver), in the unchanged manifest format, and a fresh doc load reads it back.
#[test]
fn set_group_meta_round_trips_through_flush_and_the_saver() {
    let dir = temp_dir("group_meta");
    persist::add_page_raster(&dir, None, &persist::PageRasterSpec { page_idx: 0, uid: "r", name: "Pic", visible: true, opacity: 1.0, transform: tf(1.0, 1.0, 1.0), image: &img([2, 2], Color32::RED) })
        .unwrap();
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).unwrap();
    doc.add_group(
        0,
        GroupMeta {
            uid: "g0".into(),
            name: "Group".into(),
            visible: true,
            opacity: 1.0,
            collapsed: false,
        },
    );
    doc.set_group(0, "r", Some("g0".into()));
    assert!(doc.set_group_meta(0, "g0", false, 0.25));
    doc.flush_page(0, &dir, None).unwrap();

    let reload = |dir: &std::path::Path| {
        let mut fresh = LayerDoc::new();
        fresh.ensure_page_loaded(0, dir, None, None, &psz([100, 100])).unwrap();
        fresh.page(0).unwrap().groups.clone()
    };
    let groups = reload(&dir);
    assert_eq!(groups.len(), 1);
    assert!(!groups[0].visible, "visibility round-trips through the sync flush");
    assert!((groups[0].opacity - 0.25).abs() < 1e-6, "opacity round-trips through the sync flush");

    doc.enable_background_saver(None);
    assert!(doc.set_group_meta(0, "g0", true, 0.75));
    doc.enqueue_page_save(0, &dir, None).unwrap();
    let handle = doc.saver_handle().expect("saver enabled");
    assert!(handle.barrier_blocking().is_empty(), "no failed page");
    let groups = reload(&dir);
    assert!(groups[0].visible, "visibility round-trips through the saver");
    assert!((groups[0].opacity - 0.75).abs() < 1e-6, "opacity round-trips through the saver");

    doc.shutdown_saver();
    if let Err(err) = fs::remove_dir_all(&dir) {
        eprintln!("test cleanup {}: {err}", dir.display());
    }
}

#[test]
fn set_group_then_remove_group_ungroups_members() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
    doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE));

    let group = GroupMeta {
        uid: "g0".into(),
        name: "Group".into(),
        visible: true,
        opacity: 1.0,
        collapsed: false,
    };
    doc.add_group(0, group.clone());
    assert_eq!(doc.page(0).unwrap().groups.len(), 1);
    // Duplicate uid ignored.
    doc.add_group(0, group);
    assert_eq!(
        doc.page(0).unwrap().groups.len(),
        1,
        "duplicate group uid ignored"
    );

    doc.set_group(0, "a", Some("g0".into()));
    doc.set_group(0, "b", Some("g0".into()));
    assert_eq!(doc.node(0, "a").unwrap().group_uid.as_deref(), Some("g0"));
    assert_eq!(doc.node(0, "b").unwrap().group_uid.as_deref(), Some("g0"));

    doc.remove_group(0, "g0");
    assert!(doc.page(0).unwrap().groups.is_empty(), "GroupMeta dropped");
    assert_eq!(
        doc.node(0, "a").unwrap().group_uid,
        None,
        "member a ungrouped"
    );
    assert_eq!(
        doc.node(0, "b").unwrap().group_uid,
        None,
        "member b ungrouped"
    );
}

#[test]
fn set_raster_pixels_bumps_generation_and_swaps() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
    let gen_before = doc.node(0, "a").unwrap().generation;

    let effects = vec![serde_json::json!({"type": "blur"})];
    doc.set_raster_pixels(
        0,
        "a",
        img([4, 4], Color32::GREEN),
        img([5, 5], Color32::BLUE),
        effects,
        true,
    );

    let node = doc.node(0, "a").unwrap();
    assert!(
        node.generation > gen_before,
        "set_raster_pixels bumps generation"
    );
    assert!(node.pixels_dirty, "pixels_dirty set");
    if let NodeBody::Raster {
        base_image,
        display_image,
        effects,
        ..
    } = &node.body
    {
        assert_eq!(base_image.size, [4, 4], "base swapped");
        assert_eq!(display_image.size, [5, 5], "display swapped");
        assert_eq!(effects.len(), 1, "effects swapped");
    } else {
        panic!("expected raster body");
    }

    // No-op on a text node.
    doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
    let t_gen = doc.node(0, "t").unwrap().generation;
    doc.set_raster_pixels(
        0,
        "t",
        img([9, 9], Color32::RED),
        img([9, 9], Color32::RED),
        Vec::new(),
        true,
    );
    assert_eq!(
        doc.node(0, "t").unwrap().generation,
        t_gen,
        "no-op on a text node"
    );
}

#[test]
fn set_text_render_bumps_generation_and_swaps() {
    let mut doc = doc_with_empty_page();
    doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
    let gen_before = doc.node(0, "t").unwrap().generation;

    doc.set_text_render(
        0,
        "t",
        serde_json::json!({"text": "Hi", "size": 32}),
        img([7, 8], Color32::BLUE),
        RenderedTextExtraInfo::default(),
    );

    let node = doc.node(0, "t").unwrap();
    assert!(
        node.generation > gen_before,
        "set_text_render bumps generation"
    );
    if let NodeBody::Text {
        render_data, image, ..
    } = &node.body
    {
        assert_eq!(render_data["text"], "Hi", "render_data swapped");
        assert_eq!(render_data["size"], 32);
        assert_eq!(image.size, [7, 8], "image swapped");
    } else {
        panic!("expected text body");
    }

    // No-op on a raster node.
    doc.add_node(0, raster_node("r", [2, 2], Color32::RED));
    let r_gen = doc.node(0, "r").unwrap().generation;
    doc.set_text_render(
        0,
        "r",
        serde_json::json!({"x": 1}),
        img([9, 9], Color32::RED),
        RenderedTextExtraInfo::default(),
    );
    assert_eq!(
        doc.node(0, "r").unwrap().generation,
        r_gen,
        "no-op on a raster node"
    );
}

/// A compact, comparable fingerprint of a loaded page's nodes + groups, so the split path and the
/// monolith can be asserted byte-for-byte equal without depending on `LayerNode`/`ColorImage`
/// equality (which they do not implement). Captures every field that the split must preserve.
fn page_fingerprint(page: &DocPage) -> String {
    let nodes: Vec<String> = page
        .nodes
        .iter()
        .map(|n| {
            let kind = match n.kind {
                NodeKind::Raster => "R",
                NodeKind::Text => "T",
            };
            let img = n.display_image();
            format!(
                "{kind}|uid={}|name={}|z={}|vis={}|op={:.4}|grp={:?}|tli={:?}|cx={:.4}|cy={:.4}|rot={:.4}|sc={:.4}|deform={}|wh={}x{}",
                n.uid,
                n.name,
                n.z,
                n.visible,
                n.opacity,
                n.group_uid,
                n.text_layer_idx,
                n.transform.cx,
                n.transform.cy,
                n.transform.rotation,
                n.transform.scale,
                n.deform.is_some(),
                img.size[0],
                img.size[1],
            )
        })
        .collect();
    let groups: Vec<String> = page
        .groups
        .iter()
        .map(|g| {
            format!(
                "G|uid={}|name={}|vis={}|op={:.4}|col={}",
                g.uid, g.name, g.visible, g.opacity, g.collapsed
            )
        })
        .collect();
    format!(
        "nodes=[{}] groups=[{}]",
        nodes.join(", "),
        groups.join(", ")
    )
}

/// The off-thread split (`decode_page_payload` + `insert_decoded_page`) must produce a loaded page
/// IDENTICAL to the legacy synchronous `ensure_page_loaded`, including the legacy `text_info.json`
/// migration-on-read path (which the off-thread worker now runs lock-free). Also checks the insert
/// memoization: a second insert onto an already-resident page is a no-op (live edits not clobbered).
#[test]
fn decode_then_insert_matches_ensure_page_loaded_incl_legacy_ribbon() {
    let dir = temp_dir("decode_split_eq");

    // A raster at the bottom, a PS group, and a LEGACY text group of three unpinned overlays whose
    // geometry is decoded through the shared codec with the FULL chapter page-size map — exercising
    // the same legacy migration path the worker must reproduce.
    persist::add_page_raster(
        &dir,
        None,
        &persist::PageRasterSpec {
            page_idx: 0,
            uid: "r0",
            name: "R",
            visible: true,
            opacity: 1.0,
            transform: tf(50.0, 50.0, 1.0),
            image: &img([3, 3], Color32::RED),
        },
    )
    .unwrap();
    add_text_group_band(&dir, 0, 5);
    for uid in ["tc", "ta", "tb"] {
        add_legacy_text_ref_node(&dir, uid, 0, 0, false, true, 1.0);
    }
    let mk = |uid: &str, y: f32| {
        save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
        serde_json::json!({
            "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
            "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
            "render_data": { "text": uid }
        })
    };
    let overlays = serde_json::json!([mk("ta", 10.0), mk("tb", 20.0), mk("tc", 30.0)]);
    fs::write(
        dir.join("text_info.json"),
        serde_json::to_string(&overlays).unwrap(),
    )
    .unwrap();

    // FULL chapter page-size map (a second page is present so the ribbon solve is non-degenerate).
    let mut page_sizes = HashMap::new();
    page_sizes.insert(0usize, [1000usize, 1400usize]);
    page_sizes.insert(1usize, [1000usize, 1600usize]);

    // Reference: the synchronous monolith.
    let mut doc_ref = LayerDoc::new();
    doc_ref
        .ensure_page_loaded(0, &dir, None, None, &page_sizes)
        .unwrap();
    let ref_fp = page_fingerprint(doc_ref.page(0).expect("ref page resident"));

    // Split: decode lock-free, then insert.
    let payload =
        LayerDoc::decode_page_payload(0, &dir, None, None, &page_sizes).expect("decode succeeds");
    let mut doc_split = LayerDoc::new();
    let version_before = doc_split.version();
    doc_split.insert_decoded_page(0, payload);
    let split_fp = page_fingerprint(doc_split.page(0).expect("split page resident"));

    assert_eq!(
        ref_fp, split_fp,
        "split path matches ensure_page_loaded (incl. legacy migration)"
    );
    assert!(
        doc_split.version() > version_before,
        "a real insert bumps the doc version (cross-tab re-projection)"
    );

    // MEMOIZATION: a second insert of a freshly-decoded payload must NOT clobber the resident page.
    let payload2 =
        LayerDoc::decode_page_payload(0, &dir, None, None, &page_sizes).expect("decode succeeds");
    let version_after_first = doc_split.version();
    doc_split.insert_decoded_page(0, payload2);
    assert_eq!(
        doc_split.version(),
        version_after_first,
        "insert onto an already-resident page is a no-op (does not bump version)"
    );
    assert_eq!(
        page_fingerprint(doc_split.page(0).unwrap()),
        split_fp,
        "resident page unchanged by the discarded second payload"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stale_text_ack_cannot_clear_newer_dirty_generation() {
    let dir = temp_dir("text_ack_generation");
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage {
        nodes: vec![text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.enable_background_saver(None);
    doc.set_text_render(
        0,
        "t",
        serde_json::json!({"text": "first"}),
        img([2, 2], Color32::RED),
        RenderedTextExtraInfo::default(),
    );
    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    doc.set_text_render(
        0,
        "t",
        serde_json::json!({"text": "second"}),
        img([2, 2], Color32::BLUE),
        RenderedTextExtraInfo::default(),
    );
    let handle = doc.saver_handle().expect("background saver enabled");
    assert!(handle.barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(doc.node(0, "t").unwrap().pixels_dirty, "stale success cannot clear a newer edit");

    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    assert!(handle.barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(!doc.node(0, "t").unwrap().pixels_dirty, "current successful acknowledgement settles text dirty");
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn failed_text_ack_leaves_text_dirty() {
    let invalid_parent = temp_dir("failed_text_ack_parent");
    fs::write(&invalid_parent, b"not a directory").unwrap();
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage {
        nodes: vec![text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.enable_background_saver(None);
    doc.set_text_render(
        0,
        "t",
        serde_json::json!({"text": "dirty"}),
        img([2, 2], Color32::RED),
        RenderedTextExtraInfo::default(),
    );
    doc.enqueue_page_text_save(0, &invalid_parent.join("layers"), None).unwrap();
    let handle = doc.saver_handle().expect("background saver enabled");
    assert!(handle.barrier_blocking().contains(&0));
    doc.poll_save_acks();
    assert!(doc.node(0, "t").unwrap().pixels_dirty, "failed acknowledgement leaves text dirty");
    doc.shutdown_saver();
    let _ = fs::remove_file(&invalid_parent);
}

/// Evicting a page drops its pending save epoch, so a still-unconsumed pre-eviction ack cannot
/// settle the page after it is re-inserted (and the epoch map does not leak the evicted key).
#[test]
fn evict_page_drops_pending_save_epochs() {
    let dir = temp_dir("evict_drops_epoch");
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage {
        nodes: vec![text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.enable_background_saver(None);
    // Reserve a text epoch and run the real save so an OK ack for it lands in the shared map.
    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    let handle = doc.saver_handle().expect("background saver enabled");
    assert!(handle.barrier_blocking().is_empty());
    // Evict before polling: the pending epoch entry must be dropped with the page.
    doc.evict_page(0);
    assert!(
        !doc.latest_epoch.contains_key(&(0, SaveKind::Text)),
        "evict drops the page's latest_epoch entry"
    );
    // Re-insert a fresh, dirty page 0; the still-unconsumed pre-eviction ack must NOT settle it.
    doc.pages.insert(0, DocPage {
        nodes: vec![text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.poll_save_acks();
    assert!(
        doc.node(0, "t").unwrap().pixels_dirty,
        "a pre-eviction ack cannot clear the reloaded page's dirty"
    );
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

/// Adding a dirty node advances its page-kind dirty epoch, so a stale ack for the prior epoch cannot
/// clear the freshly added node's dirty.
#[test]
fn add_node_bumps_dirty_epoch_so_stale_ack_cannot_settle() {
    let dir = temp_dir("add_node_bumps_epoch");
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage {
        nodes: vec![text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.enable_background_saver(None);
    // Reserve e1 and run the real save for the existing text; its OK ack lands in the map.
    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    let handle = doc.saver_handle().expect("background saver enabled");
    assert!(handle.barrier_blocking().is_empty());
    let e1 = doc.latest_epoch.get(&(0, SaveKind::Text)).copied().expect("text epoch reserved");
    // Add a fresh DIRTY text node: this must advance the (0, Text) dirty epoch past e1.
    assert!(doc.add_node(0, text_node_with_payload("t2")));
    let e2 = doc.latest_epoch.get(&(0, SaveKind::Text)).copied().expect("text epoch present after add");
    assert!(e2 > e1, "add_node of a dirty text node advances the dirty epoch");
    // The pre-add OK ack (epoch e1) must NOT settle the page now that latest_epoch = e2.
    doc.poll_save_acks();
    assert!(
        doc.node(0, "t2").unwrap().pixels_dirty,
        "a stale (e1) ack cannot clear the node added at e2"
    );
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

/// `enqueue_pages_text_save` hands every resident page to the saver as ONE message, so the whole
/// batch lands in one drain pass: one staging manifest write for K pages, all pages persisted.
#[test]
fn batched_text_save_is_one_manifest_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let committed = tmp.path().join("ch").join("layers");
    let staging = tmp.path().join("ch_unsaved").join("layers");
    fs::create_dir_all(&committed).expect("mkdir");
    let committed_doc = ms_docstore::DocRef::new(committed.join("layers.json"), ms_docstore::DocKind::Layers).with_new_format(ms_docstore::DocFormat::Json);
    ms_docstore::write_value(&committed_doc, &serde_json::json!({"schema_version": 4, "pages": []}), ms_docstore::WriteOptions::default()).expect("seed");

    let mut doc = LayerDoc::new();
    let pages: Vec<usize> = (0..6).collect();
    for &p in &pages {
        doc.pages.insert(p, DocPage { nodes: vec![text_node_with_payload(&format!("t{p}"))], groups: Vec::new() });
    }
    doc.enable_background_saver(None);
    let results = doc.enqueue_pages_text_save(&pages, &staging, Some(&committed));
    assert!(results.iter().all(|(_, r)| r.is_ok()));
    assert_eq!(results.iter().map(|(p, _)| *p).collect::<Vec<_>>(), pages);
    let handle = doc.saver_handle().expect("saver");
    assert!(handle.barrier_blocking().is_empty());
    let writes = ms_docstore::recorded_steps(&staging.join("layers.json")).iter().filter(|s| **s == ms_docstore::WriteStep::Renamed).count();
    assert_eq!(writes, 1, "six pages sent as one batch must be one manifest write");
    for &p in &pages {
        let uids: Vec<String> = persist::load_page_text_nodes(&staging, None, p).expect("load").into_iter().map(|n| n.uid).collect();
        assert_eq!(uids, vec![format!("t{p}")]);
    }
    doc.shutdown_saver();
}

/// Without a saver the batch falls back to one synchronous flush per page, reporting each result.
#[test]
fn batched_text_save_without_saver_flushes_each_page() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut doc = LayerDoc::new();
    doc.pages.insert(2, DocPage { nodes: vec![text_node_with_payload("t")], groups: Vec::new() });
    let results = doc.enqueue_pages_text_save(&[2, 7], tmp.path(), None);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, r)| r.is_ok()), "a non-resident page is a no-op, not an error");
    assert_eq!(persist::load_page_text_nodes(tmp.path(), None, 2).expect("load").len(), 1);
}

/// A FAILED raster acknowledgement for the page's current epoch re-dirties every resident raster
/// (the retry must carry pixels, which a clean raster's job omits) and keeps the page pending; a
/// stale failure does nothing, and the current success settles both.
#[test]
fn failed_raster_ack_redirties_rasters_and_keeps_pending() {
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage {
        nodes: vec![raster_node("r", [2, 2], Color32::RED), text_node_with_payload("t")],
        groups: Vec::new(),
    });
    doc.node_mut(0, "t").expect("text node").pixels_dirty = false;
    let acks = Arc::new(Mutex::new(SaveAckMap::default()));
    doc.save_acks = Some(Arc::clone(&acks));
    assert!(!doc.has_pending_saves(), "a fresh doc has nothing pending");

    let stale = doc.next_save_epoch(0, SaveKind::Raster);
    let current = doc.next_save_epoch(0, SaveKind::Raster);
    assert!(doc.has_pending_saves());
    acks.lock().expect("acks").record(0, SaveKind::Raster, stale, false);
    doc.poll_save_acks();
    assert!(!doc.node(0, "r").expect("raster").pixels_dirty, "a stale failure does not re-dirty");

    acks.lock().expect("acks").record(0, SaveKind::Raster, current, false);
    doc.poll_save_acks();
    assert!(doc.node(0, "r").expect("raster").pixels_dirty, "current failure re-dirties the raster");
    assert!(!doc.node(0, "t").expect("text").pixels_dirty, "text nodes are untouched");
    assert!(doc.has_pending_saves(), "a failed save stays pending");

    acks.lock().expect("acks").record(0, SaveKind::Raster, current, true);
    doc.poll_save_acks();
    assert!(!doc.node(0, "r").expect("raster").pixels_dirty, "the current success settles the raster");
    assert!(!doc.has_pending_saves(), "nothing pending after the success");
}

/// The background job builder carries base pixels only for a dirty raster (a clean one's PNG is
/// already recorded), but always its size.
#[test]
fn raster_save_part_carries_pixels_only_when_dirty() {
    let mut page = DocPage { nodes: vec![raster_node("clean", [2, 2], Color32::RED), raster_node("dirty", [2, 2], Color32::BLUE)], groups: Vec::new() };
    page.nodes[1].pixels_dirty = true;
    let part = LayerDoc::build_raster_save_part(&page, &[]);
    let carried: Vec<(&str, bool, [usize; 2])> = part.layers.iter().map(|l| (l.uid.as_str(), l.base_image.is_some(), l.image_size)).collect();
    assert_eq!(carried, vec![("clean", false, [2, 2]), ("dirty", true, [2, 2])]);
}

// ---- Structural edits as saver jobs + doc-owned PS text pin meta -------------------------------

/// Writes a committed text node `uid` on `page` (with its rendered PNG) whose PS pin meta says the
/// pin is owned by grouping.
fn write_committed_text(dir: &Path, page: usize, uid: &str, z: u32, pinned_by_group: bool) {
    fs::create_dir_all(dir).expect("mkdir");
    let file = persist::write_text_image(dir, page, uid, &img([2, 2], Color32::GREEN)).expect("text png");
    let out = persist::TextPayloadOut {
        uid: uid.into(),
        name: uid.into(),
        z,
        layer_idx: 0,
        pinned: true,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        pinned_by_group,
        payload_uid: uid.into(),
        render_data: serde_json::json!({ "text": uid }),
        is_image: false,
        transform: tf(1.0, 1.0, 1.0),
        deform: None,
        rendered_file: Some(file),
        mask_clip: None,
        text_centers: None,
        centering_frame: None,
    };
    persist::write_page_text_payload(dir, None, page, &[out]).expect("write committed text");
}

fn disk_pin_meta(dir: &Path, page: usize, uid: &str) -> Option<(bool, bool)> {
    persist::load_page_text_nodes(dir, None, page).expect("load").into_iter().find(|n| n.uid == uid).map(|n| (n.pinned, n.pinned_by_group))
}

/// The PS pin meta round-trips disk → doc node → disk: decode fills `text_pinned` /
/// `text_pinned_by_group` from the record, `set_text_pin_meta` edits them (bumping the version
/// only on a change, refusing a raster), and the text flush writes the node's `pinned_by_group`.
#[test]
fn text_pin_meta_round_trips_decode_node_write() {
    let dir = temp_dir("pin_meta");
    write_committed_text(&dir, 0, "t", 1, true);
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).expect("load");
    let node = doc.node(0, "t").expect("text node");
    assert_eq!((node.text_pinned, node.text_pinned_by_group), (true, true), "decoded from the record");

    let v0 = doc.version();
    assert!(doc.set_text_pin_meta(0, "t", true, true));
    assert_eq!(doc.version(), v0, "an unchanged pin meta does not bump the version");
    assert!(doc.set_text_pin_meta(0, "t", true, false));
    assert!(doc.version() > v0, "a changed pin meta bumps the version");
    assert!(!doc.set_text_pin_meta(0, "missing", true, true));
    assert!(doc.add_node(0, raster_node("r", [2, 2], Color32::RED)));
    assert!(!doc.set_text_pin_meta(0, "r", true, true), "a raster carries no pin meta");

    doc.flush_page_text(0, &dir, None).expect("flush");
    assert_eq!(disk_pin_meta(&dir, 0, "t"), Some((true, false)), "the doc's by-group flag was written");
    let mut reloaded = LayerDoc::new();
    reloaded.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).expect("reload");
    let node = reloaded.node(0, "t").expect("text node");
    assert_eq!((node.text_pinned, node.text_pinned_by_group), (true, false));
    let _ = fs::remove_dir_all(&dir);
}

/// The queued raster delete removes the uid on disk (mirror of the synchronous
/// `flush_page_dropping_raster_removes_it_from_disk`), and survives a later coalesced save.
#[test]
fn enqueue_page_save_dropping_raster_removes_it_from_disk() {
    let dir = temp_dir("enqueue_drop_raster");
    for (uid, c) in [("r0", Color32::RED), ("r1", Color32::BLUE)] {
        persist::add_page_raster(&dir, None, &persist::PageRasterSpec { page_idx: 0, uid, name: uid, visible: true, opacity: 1.0, transform: tf(0.0, 0.0, 1.0), image: &img([2, 2], c) }).expect("seed raster");
    }
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).expect("load");
    doc.enable_background_saver(None);
    assert!(doc.remove_node(0, "r0"));
    doc.enqueue_page_save_dropping_raster(0, &dir, None, "r0").expect("enqueue");
    doc.enqueue_page_save(0, &dir, None).expect("later save of the page");
    assert!(doc.saver_handle().expect("saver").barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(!doc.has_pending_saves(), "both saves acknowledged");

    let mut reloaded = LayerDoc::new();
    reloaded.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).expect("reload");
    assert!(reloaded.node(0, "r0").is_none(), "the deleted raster did not resurrect on disk");
    assert!(reloaded.node(0, "r1").is_some(), "the other raster survives");
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

/// A structural-only job reserves its OWN epoch kind: its successful acknowledgement retires the
/// structural epoch but never settles a raster that was dirtied and not yet written (a raster ack
/// would have cleared its `pixels_dirty`). The band order itself lands on disk.
#[test]
fn structural_ack_does_not_settle_unwritten_raster_pixels() {
    let dir = temp_dir("structural_ack");
    for uid in ["a", "b"] {
        persist::add_page_raster(&dir, None, &persist::PageRasterSpec { page_idx: 0, uid, name: uid, visible: true, opacity: 1.0, transform: tf(0.0, 0.0, 1.0), image: &img([2, 2], Color32::RED) }).expect("seed raster");
    }
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100])).expect("load");
    doc.enable_background_saver(None);
    doc.node_mut(0, "a").expect("raster").pixels_dirty = true;
    doc.bump_dirty_epoch(0, SaveKind::Raster);

    doc.enqueue_page_band_order(0, &dir, None, vec![BandRef::Raster("b".into()), BandRef::Raster("a".into())]).expect("enqueue");
    assert!(doc.page_has_pending_save(0));
    assert!(doc.saver_handle().expect("saver").barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(doc.node(0, "a").expect("raster").pixels_dirty, "unwritten pixels stay dirty");
    assert!(doc.latest_epoch.contains_key(&(0, SaveKind::Raster)), "the raster edit is still pending");
    assert!(!doc.latest_epoch.contains_key(&(0, SaveKind::Structural)), "the structural edit is acknowledged");
    let order: Vec<String> = persist::load_page_rasters(&dir, None, 0).expect("load").layers.into_iter().map(|l| l.uid).collect();
    assert_eq!(order, vec!["b", "a"], "the band order was written");

    // A structural edit for a page the doc never loaded is acknowledged too, so it cannot read as
    // pending forever.
    doc.enqueue_page_grouping(7, &dir, None, GroupingEdit::default()).expect("enqueue");
    assert!(doc.saver_handle().expect("saver").barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(!doc.latest_epoch.contains_key(&(7, SaveKind::Structural)), "a non-resident page's structural epoch retires");
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

/// Final staging manifest + staged PNGs of creating raster `img1` on committed-typeset page 3,
/// either the historical way (flush the doc text, `persist::add_page_raster`, reload, flush text)
/// or doc-first (`add_node` a dirty raster, `enqueue_page_save` through the saver, flush text).
/// `delete_text` first deletes the page's only text in the doc. Also returns the committed text
/// count after the save-to-project merge.
fn create_raster_outcome(root: &Path, doc_first: bool, delete_text: bool) -> (super::super::manifest::LayersManifest, Vec<(String, Vec<u8>)>, usize) {
    let committed = root.join("ch").join("layers");
    let unsaved = root.join("ch_unsaved").join("layers");
    write_committed_text(&committed, 3, "t3", 1, false);
    let sizes: HashMap<usize, [usize; 2]> = (0..=3).map(|p| (p, [2, 2])).collect();
    let pic = img([2, 2], Color32::BLUE);
    let transform = tf(1.0, 1.0, 1.0);
    let mut doc = LayerDoc::new();
    doc.ensure_page_loaded(3, &unsaved, Some(&committed), None, &sizes).expect("load");
    if delete_text {
        assert!(doc.remove_node(3, "t3"));
    }
    if doc_first {
        doc.enable_background_saver(None);
        let node = LayerNode {
            pixels_dirty: true,
            transform,
            name: "Картинка".into(),
            body: NodeBody::Raster {
                base_image: pic.clone(),
                display_image: pic,
                effects: Vec::new(),
                base_file: persist::base_file_name(3, "img1"),
                mask_clip: None,
            },
            ..raster_node("img1", [2, 2], Color32::BLUE)
        };
        assert!(doc.add_node(3, node));
        let top = doc.page(3).expect("page").nodes.last().map(|n| n.uid.clone());
        assert_eq!(top.as_deref(), Some("img1"), "the new raster lands on top");
        doc.enqueue_page_save(3, &unsaved, Some(&committed)).expect("enqueue");
        assert!(doc.saver_handle().expect("saver").barrier_blocking().is_empty());
        doc.poll_save_acks();
        assert!(!doc.has_pending_saves());
    } else {
        doc.flush_page_text(3, &unsaved, Some(&committed)).expect("pre-flush text");
        persist::add_page_raster(&unsaved, Some(&committed), &persist::PageRasterSpec { page_idx: 3, uid: "img1", name: "Картинка", visible: true, opacity: 1.0, transform, image: &pic }).expect("add raster");
        doc.evict_page(3);
        doc.ensure_page_loaded(3, &unsaved, Some(&committed), None, &sizes).expect("reload");
    }
    doc.flush_page_text(3, &unsaved, Some(&committed)).expect("save-to-project text flush");
    doc.shutdown_saver();
    let manifest = super::super::compat::read_manifest(&unsaved.join("layers.json")).expect("read").expect("staged manifest");
    let mut pngs: Vec<(String, Vec<u8>)> = fs::read_dir(&unsaved)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .map(|p| (p.file_name().expect("name").to_string_lossy().into_owned(), fs::read(&p).expect("png")))
        .collect();
    pngs.sort();
    let owned: std::collections::HashSet<usize> = [3].into_iter().collect();
    persist::merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).expect("merge");
    let committed_texts = persist::load_page_text_nodes(&committed, None, 3).expect("load").len();
    (manifest, pngs, committed_texts)
}

/// Doc-first raster creation (the typing create path after A2-TY) persists what
/// `persist::add_page_raster` did: on a typeset page the staged manifest and PNGs are IDENTICAL and
/// the committed text survives the merge; on a page whose last text was deleted the deletion stays
/// durable and the page holds exactly the new raster. (There the raster's absolute `z` differs —
/// the doc-first job writes rasters before text, so the raster is placed above the still-seeded
/// committed text before the text half empties it — which is only a rank, not an order change.)
#[test]
fn doc_first_raster_create_matches_add_page_raster() {
    use super::super::manifest::{LayerKindRec, LayersManifest};
    let old = tempfile::tempdir().expect("tempdir");
    let new = tempfile::tempdir().expect("tempdir");
    let (old_manifest, old_pngs, old_texts) = create_raster_outcome(old.path(), false, false);
    let (new_manifest, new_pngs, new_texts) = create_raster_outcome(new.path(), true, false);
    assert_eq!(serde_json::to_value(&old_manifest).expect("json"), serde_json::to_value(&new_manifest).expect("json"), "typeset page: identical staged manifest");
    assert_eq!(old_pngs, new_pngs, "typeset page: identical staged PNGs");
    assert_eq!((old_texts, new_texts), (1, 1), "committed text survives both paths");

    let old = tempfile::tempdir().expect("tempdir");
    let new = tempfile::tempdir().expect("tempdir");
    let (old_manifest, old_pngs, old_texts) = create_raster_outcome(old.path(), false, true);
    let (new_manifest, new_pngs, new_texts) = create_raster_outcome(new.path(), true, true);
    let shape = |m: &LayersManifest| -> Vec<(String, LayerKindRec)> {
        let mut tree: Vec<_> = m.page(3).expect("page 3 staged").tree.iter().map(|r| (r.z, r.uid.clone(), r.kind)).collect();
        tree.sort_by_key(|(z, _, _)| *z);
        tree.into_iter().map(|(_, uid, kind)| (uid, kind)).collect()
    };
    assert_eq!(shape(&old_manifest), shape(&new_manifest), "deleted-last-text page: same nodes in the same order");
    assert_eq!(shape(&new_manifest), vec![("img1".to_string(), LayerKindRec::Raster)]);
    assert_eq!(old_pngs, new_pngs, "deleted-last-text page: identical staged PNGs");
    assert_eq!((old_texts, new_texts), (0, 0), "the deleted text is not resurrected by either path");
}

// ---- Autosave gate --------------------------------------------------------------------------

fn autosave_test_gate(action_threshold: u32) -> Arc<AutosaveGate> {
    let interval = std::time::Duration::from_secs(3600);
    Arc::new(AutosaveGate::with_policy_fn(move || ms_config::autosave_policy::AutosavePolicy { interval, action_threshold }))
}

/// A text save held by the gate keeps the page pending (the exit dialog must see it) until a
/// barrier forces it out and the acknowledgement settles it.
#[test]
fn held_save_stays_pending_until_the_barrier_writes_it() {
    let dir = temp_dir("autosave_held_pending");
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage { nodes: vec![text_node_with_payload("t")], groups: Vec::new() });
    doc.enable_background_saver(Some(autosave_test_gate(1000)));
    doc.set_text_render(0, "t", serde_json::json!({"text": "held"}), img([2, 2], Color32::RED), RenderedTextExtraInfo::default());
    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    doc.poll_save_acks();
    assert!(doc.has_pending_saves(), "a held job must read as pending");
    assert!(doc.node(0, "t").unwrap().pixels_dirty);
    let handle = doc.saver_handle().expect("background saver enabled");
    assert!(handle.barrier_blocking().is_empty());
    doc.poll_save_acks();
    assert!(!doc.has_pending_saves(), "the barrier wrote and acknowledged the held job");
    assert!(!doc.node(0, "t").unwrap().pixels_dirty);
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}

/// Every funnel call is ONE gesture: with a threshold of 2, one enqueue is held and the second
/// (a different funnel) closes the window, so both land without a barrier.
#[test]
fn each_funnel_call_reports_one_autosave_action() {
    let dir = temp_dir("autosave_funnel_actions");
    let gate = autosave_test_gate(2);
    let mut doc = LayerDoc::new();
    doc.pages.insert(0, DocPage { nodes: vec![text_node_with_payload("t")], groups: Vec::new() });
    doc.enable_background_saver(Some(Arc::clone(&gate)));
    let start_epoch = gate.poll();
    doc.set_text_render(0, "t", serde_json::json!({"text": "a"}), img([2, 2], Color32::RED), RenderedTextExtraInfo::default());
    doc.enqueue_page_text_save(0, &dir, None).unwrap();
    assert_eq!(gate.poll(), start_epoch, "one action of two does not close the window");
    doc.note_autosave_action();
    assert_eq!(gate.poll(), start_epoch + 1, "the second action closes the window");
    let written = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        doc.poll_save_acks();
        !doc.has_pending_saves()
    });
    assert!(written, "the due window must write the held job without a barrier");
    doc.shutdown_saver();
    let _ = fs::remove_dir_all(&dir);
}
