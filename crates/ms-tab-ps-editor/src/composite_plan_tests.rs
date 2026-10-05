/*
File: crates/ms-tab-ps-editor/src/composite_plan_tests.rs

Purpose:
Tests of the PS canvas draw plan `composite_steps` (crate root, `lib.rs`): the order and effective
opacities `draw_composite` paints above the base layers.

Notes:
`composite_steps` is a thin adapter over the shared owner
`ms_models::layer_model::ordering::composite_plan`, so these tests pin the PS-side ADAPTATION (stack
views, text views, group fold source, index mapping) against the owner's contract: equal-rank ties
resolve by input order (stack order for rasters, `text_layers` order for texts) and a raster sits
below a text at equal band Z (change B3 of `dev-docs/single_image_mode_plan.md`); the text band
lookup is uid-first. Pure: no files, no GPU, no egui context.
*/

use super::*;
use egui::Color32;
use ms_models::layer_model::layer_doc::{DocPage, LayerNode, NodeBody, NodeKind};

/// A small page-sized stack with only the two base layers.
fn stack() -> LayerStack {
    let size = [2, 2];
    let img = ColorImage::filled(size, Color32::TRANSPARENT);
    LayerStack::new(0, size, img.clone(), img)
}

/// A text view with the fields the plan does not vary per test set to neutral values.
fn text<'a>(index: usize, uid: &'a str, layer_idx: u32) -> PsTextView<'a> {
    PsTextView {
        index,
        uid,
        layer_idx,
        visible: true,
        group_uid: None,
    }
}

/// The stack's unified groups as owned `(uid, visible, opacity)`, the shape `draw_composite`
/// stringifies them into before borrowing them as [`GroupFold`]s.
fn stack_groups(stack: &LayerStack) -> Vec<(String, bool, f32)> {
    stack
        .groups()
        .iter()
        .map(|g| (g.uid.to_string(), g.visible, g.opacity))
        .collect()
}

/// Borrowed fold views of owned `(uid, visible, opacity)` groups.
fn folds(groups: &[(String, bool, f32)]) -> Vec<GroupFold<'_>> {
    groups
        .iter()
        .map(|(uid, visible, opacity)| GroupFold {
            uid,
            visible: *visible,
            opacity: *opacity,
        })
        .collect()
}

/// The `PsStep`s of a plan, without the band Z.
fn steps(plan: &[(u32, PsStep)]) -> Vec<PsStep> {
    plan.iter().map(|(_, step)| *step).collect()
}

/// The text indices of a plan, bottom-to-top.
fn text_order(plan: &[(u32, PsStep)]) -> Vec<usize> {
    plan.iter()
        .filter_map(|(_, step)| match step {
            PsStep::Text { index, .. } => Some(*index),
            PsStep::Raster { .. } => None,
        })
        .collect()
}

/// A 1x1 doc node of `kind` at unified `z`; a text node carries the given doc-owned pin flag.
fn doc_node(uid: &str, kind: NodeKind, z: u32, pinned: bool) -> LayerNode {
    let image = ColorImage::filled([1, 1], Color32::WHITE);
    let body = match kind {
        NodeKind::Raster => NodeBody::Raster {
            base_image: image.clone(),
            display_image: image,
            effects: Vec::new(),
            base_file: format!("{uid}.png"),
            mask_clip: None,
        },
        NodeKind::Text => NodeBody::Text {
            render_data: serde_json::Value::Null,
            image,
            is_image: false,
            payload_uid: uid.to_owned(),
            mask_clip: None,
            extra_centers: Default::default(),
            centering_frame: None,
        },
    };
    LayerNode {
        uid: uid.to_owned(),
        name: uid.to_owned(),
        kind,
        z,
        visible: true,
        opacity: 1.0,
        group_uid: None,
        text_layer_idx: (kind == NodeKind::Text).then_some(0),
        text_pinned: kind == NodeKind::Text && pinned,
        text_pinned_by_group: false,
        transform: TransformRec {
            cx: 0.0,
            cy: 0.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        generation: 0,
        pixels_dirty: false,
        body,
    }
}

/// Rasters take the group fold from the stack's groups: a hidden group drops its member, a dimmed
/// group multiplies the member's own opacity, and an own opacity of zero drops the layer. Base
/// layers never become views; a view carries the layer's OWN visibility / opacity.
#[test]
fn raster_group_hidden_and_dimmed() {
    let mut stack = stack();
    let hidden = stack.add_raster_layer();
    let dimmed = stack.add_raster_layer();
    let plain = stack.add_raster_layer();
    let transparent = stack.add_raster_layer();
    let hidden_group = stack.add_group("hidden".to_owned());
    let dim_group = stack.add_group("dim".to_owned());
    stack.set_layer_group(hidden, Some(hidden_group));
    stack.set_layer_group(dimmed, Some(dim_group));
    if let Some(group) = stack.group_mut(hidden_group) {
        group.visible = false;
    }
    if let Some(group) = stack.group_mut(dim_group) {
        group.opacity = 0.5;
    }
    if let Some(layer) = stack.layer_mut(dimmed) {
        layer.opacity = 0.5;
    }
    if let Some(layer) = stack.layer_mut(transparent) {
        layer.opacity = 0.0;
    }

    let rasters = PsRasterView::collect(&stack);
    assert_eq!(
        rasters.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![hidden, dimmed, plain, transparent],
        "views cover the user rasters only, in stack order"
    );
    assert!(
        rasters.iter().all(|r| r.visible),
        "a view keeps the layer's own visibility; the group fold is the owner's"
    );
    let bands: Vec<Band> = rasters
        .iter()
        .zip(0u32..)
        .map(|(r, z)| Band::Raster {
            uid: r.uid.clone(),
            z,
        })
        .collect();

    let groups = stack_groups(&stack);
    let plan = composite_steps(&rasters, &[], &bands, &folds(&groups));
    assert_eq!(
        steps(&plan),
        vec![
            PsStep::Raster {
                id: dimmed,
                opacity: 0.25
            },
            PsStep::Raster {
                id: plain,
                opacity: 1.0
            },
        ]
    );
}

/// Texts take the group fold from `groups`: a hidden group or a zero-opacity group drops the text,
/// a dimmed group sets the step opacity, an unknown group uid is ignored, and an invisible text is
/// dropped regardless of its group.
#[test]
fn text_group_hidden_and_dimmed() {
    let groups = vec![
        ("g-hidden".to_owned(), false, 1.0),
        ("g-dim".to_owned(), true, 0.25),
        ("g-zero".to_owned(), true, 0.0),
    ];
    let mut in_hidden = text(0, "t0", 0);
    in_hidden.group_uid = Some("g-hidden");
    let mut in_dim = text(1, "t1", 0);
    in_dim.group_uid = Some("g-dim");
    let mut in_zero = text(2, "t2", 0);
    in_zero.group_uid = Some("g-zero");
    let mut in_unknown = text(3, "t3", 0);
    in_unknown.group_uid = Some("g-missing");
    let mut invisible = text(4, "t4", 0);
    invisible.visible = false;
    let ungrouped = text(5, "t5", 0);

    let texts = [in_hidden, in_dim, in_zero, in_unknown, invisible, ungrouped];
    let plan = composite_steps(&[], &texts, &[], &folds(&groups));
    assert_eq!(
        steps(&plan),
        vec![
            PsStep::Text {
                index: 1,
                opacity: 0.25
            },
            PsStep::Text {
                index: 3,
                opacity: 1.0
            },
            PsStep::Text {
                index: 5,
                opacity: 1.0
            },
        ]
    );
}

/// The text band lookup is uid-first: a `PinnedText` band naming the text's uid wins; without one
/// the text falls back to its legacy `TextGroup` band by `layer_idx`; with neither it sits on top.
#[test]
fn text_band_lookup_is_uid_first() {
    let bands = vec![
        Band::TextGroup {
            layer_idx: 7,
            z: 0,
            member_uids: Vec::new(),
        },
        Band::PinnedText {
            uid: "own-band".to_owned(),
            z: 1,
        },
    ];
    let texts = [
        text(0, "own-band", 7),
        text(1, "group-only", 7),
        text(2, "no-band", 9),
    ];

    let plan = composite_steps(&[], &texts, &bands, &[]);
    let z_of = |index: usize| {
        plan.iter()
            .find(|(_, step)| matches!(step, PsStep::Text { index: i, .. } if *i == index))
            .map(|(z, _)| *z)
    };
    assert_eq!(z_of(0), Some(1), "uid => its PinnedText band, not its text group's");
    assert_eq!(z_of(1), Some(0), "no PinnedText band => its TextGroup band by layer_idx");
    assert_eq!(z_of(2), Some(2), "neither => top (bands.len())");
}

/// On doc-derived bands (`ordering::doc_page_bands`, the only bands `sync_view_from_doc` builds)
/// every text node has its own `PinnedText` band, so a text draws at its node Z WHETHER OR NOT its
/// PS pin flag is set. A pinned text was already drawn there; an UNPINNED doc text (PS unpin, or a
/// legacy not-yet-resaved chapter) used to fall to the top, because the old lookup chose the
/// `TextGroup` table by the pin flag and doc bands hold no `TextGroup`.
#[test]
fn doc_bands_place_pinned_and_unpinned_texts_at_their_node_z() {
    let page = DocPage {
        nodes: vec![
            doc_node("pinned", NodeKind::Text, 0, true),
            doc_node("raster", NodeKind::Raster, 1, false),
            doc_node("unpinned", NodeKind::Text, 2, false),
            doc_node("raster-top", NodeKind::Raster, 3, false),
        ],
        groups: Vec::new(),
    };
    let bands = ordering::doc_page_bands(&page);

    let mut stack = stack();
    let raster = stack.add_raster_layer();
    let raster_top = stack.add_raster_layer();
    let mut rasters = PsRasterView::collect(&stack);
    for (view, uid) in rasters.iter_mut().zip(["raster", "raster-top"]) {
        uid.clone_into(&mut view.uid);
    }
    // `text_layers` order is doc order; the pin flag is irrelevant to the lookup.
    let texts = [text(0, "pinned", 0), text(1, "unpinned", 0)];

    let plan = composite_steps(&rasters, &texts, &bands, &[]);
    assert_eq!(
        plan,
        vec![
            (
                0,
                PsStep::Text {
                    index: 0,
                    opacity: 1.0
                }
            ),
            (
                1,
                PsStep::Raster {
                    id: raster,
                    opacity: 1.0
                }
            ),
            (
                2,
                PsStep::Text {
                    index: 1,
                    opacity: 1.0
                }
            ),
            (
                3,
                PsStep::Raster {
                    id: raster_top,
                    opacity: 1.0
                }
            ),
        ]
    );
}

/// An item without a band sits at `bands.len()`, above every band; with no bands that is 0.
#[test]
fn missing_band_sits_on_top() {
    let mut stack = stack();
    let banded = stack.add_raster_layer();
    let unbanded = stack.add_raster_layer();
    let rasters = PsRasterView::collect(&stack);
    let banded_uid = rasters
        .iter()
        .find(|r| r.id == banded)
        .map(|r| r.uid.clone())
        .unwrap_or_default();
    let bands = vec![
        Band::Raster {
            uid: banded_uid,
            z: 0,
        },
        Band::TextGroup {
            layer_idx: 1,
            z: 1,
            member_uids: Vec::new(),
        },
    ];
    let texts = [text(0, "grouped", 1), text(1, "orphan", 9)];

    let plan = composite_steps(&rasters, &texts, &bands, &[]);
    assert_eq!(
        plan,
        vec![
            (
                0,
                PsStep::Raster {
                    id: banded,
                    opacity: 1.0
                }
            ),
            (
                1,
                PsStep::Text {
                    index: 0,
                    opacity: 1.0
                }
            ),
            (
                2,
                PsStep::Raster {
                    id: unbanded,
                    opacity: 1.0
                }
            ),
            (
                2,
                PsStep::Text {
                    index: 1,
                    opacity: 1.0
                }
            ),
        ]
    );

    let no_bands = composite_steps(&rasters, &texts, &[], &[]);
    assert!(no_bands.iter().all(|(z, _)| *z == 0), "empty bands => top is 0");
}

/// B3: among texts at the same band Z, the order is `text_layers` (input) order — newest last = on
/// top — never page-Y.
#[test]
fn same_z_texts_keep_input_order() {
    let bands = vec![Band::TextGroup {
        layer_idx: 0,
        z: 0,
        member_uids: Vec::new(),
    }];
    let texts = [text(0, "low", 0), text(1, "high", 0), text(2, "mid", 0)];
    let plan = composite_steps(&[], &texts, &bands, &[]);
    assert_eq!(text_order(&plan), vec![0, 1, 2]);
}

/// B3: at equal band Z (here all unbanded) a raster always draws below every text, whatever the
/// text's position on the page; the texts keep their input order above it.
#[test]
fn same_z_raster_sits_below_texts() {
    let mut stack = stack();
    let raster = stack.add_raster_layer();
    let rasters = PsRasterView::collect(&stack);
    let texts = [text(0, "first", 0), text(1, "second", 0)];
    let plan = composite_steps(&rasters, &texts, &[], &[]);
    assert_eq!(
        steps(&plan),
        vec![
            PsStep::Raster {
                id: raster,
                opacity: 1.0
            },
            PsStep::Text {
                index: 0,
                opacity: 1.0
            },
            PsStep::Text {
                index: 1,
                opacity: 1.0
            },
        ]
    );
}

/// Steps map back to the right raster id / `text_layers` index even when `PsTextView::index` is
/// not the view's position (a caller may pass a filtered text list).
#[test]
fn steps_map_back_to_view_identities() {
    let mut stack = stack();
    let raster = stack.add_raster_layer();
    let rasters = PsRasterView::collect(&stack);
    let texts = [text(4, "a", 0), text(9, "b", 0)];
    let plan = composite_steps(&rasters, &texts, &[], &[]);
    assert_eq!(text_order(&plan), vec![4, 9]);
    assert!(plan
        .iter()
        .any(|(_, step)| *step == PsStep::Raster { id: raster, opacity: 1.0 }));
}

// ---- layers panel tree == composite order (B3 / B4) ----

/// One drawable identity, comparable across the tree and the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Raster(LayerId),
    Text(usize),
}

/// A PS text runtime with `uid`, legacy `layer_idx`, pin flag and page-Y centre.
fn text_layer(uid: &str, layer_idx: u32, pinned: bool, center_y: f32) -> PsTextLayer {
    let mut layer =
        PsTextLayer::meta_from_node(uid.to_owned(), uid.to_owned(), layer_idx, None, pinned, false);
    // `meta_from_node` starts at the origin; `translate` is the public way to move it.
    layer.translate(Vec2::new(0.0, center_y));
    layer
}

/// The panel's user rows bottom-to-top (base rows and group headers dropped).
fn tree_rows(stack: &LayerStack, texts: &[PsTextLayer], bands: &[Band]) -> Vec<Row> {
    let mut rows: Vec<Row> = tree::build_unified_tree(stack, texts, bands)
        .into_iter()
        .filter_map(|item| match item {
            tree::TreeItem::Leaf(tree::Leaf {
                kind: tree::LeafKind::Raster(id),
                ..
            }) => Some(Row::Raster(id)),
            tree::TreeItem::Leaf(tree::Leaf {
                kind: tree::LeafKind::Text(index),
                ..
            }) => Some(Row::Text(index)),
            tree::TreeItem::Leaf(tree::Leaf {
                kind: tree::LeafKind::Base(_),
                ..
            })
            | tree::TreeItem::Group(_) => None,
        })
        .collect();
    rows.reverse();
    rows
}

/// The canvas draw order bottom-to-top, built exactly like `draw_composite` builds it.
fn composite_rows(stack: &LayerStack, texts: &[PsTextLayer], bands: &[Band]) -> Vec<Row> {
    let group_uids = stack_group_uids(stack);
    let groups = stack_group_folds(stack, &group_uids);
    let rasters = PsRasterView::collect(stack);
    let views: Vec<PsTextView<'_>> = texts
        .iter()
        .enumerate()
        .map(|(index, layer)| PsTextView::of(index, layer))
        .collect();
    composite_steps(&rasters, &views, bands, &groups)
        .into_iter()
        .map(|(_, step)| match step {
            PsStep::Raster { id, .. } => Row::Raster(id),
            PsStep::Text { index, .. } => Row::Text(index),
        })
        .collect()
}

/// B4: on doc-derived bands an UNPINNED text sits at its node Z in the panel exactly as on the
/// canvas (the panel used to list it on top, choosing the band table by the pin flag).
#[test]
fn tree_matches_composite_for_unpinned_doc_text() {
    let mut stack = stack();
    let low = stack.add_raster_layer();
    let high = stack.add_raster_layer();
    let uid = |id: LayerId, stack: &LayerStack| stack.layer(id).map(|l| l.uid.to_string()).unwrap_or_default();
    let page = DocPage {
        nodes: vec![
            doc_node("pinned", NodeKind::Text, 0, true),
            doc_node(&uid(low, &stack), NodeKind::Raster, 1, false),
            doc_node("unpinned", NodeKind::Text, 2, false),
            doc_node(&uid(high, &stack), NodeKind::Raster, 3, false),
        ],
        groups: Vec::new(),
    };
    let bands = ordering::doc_page_bands(&page);
    let texts = [text_layer("pinned", 0, true, 0.0), text_layer("unpinned", 0, false, 0.0)];

    let expected = vec![Row::Text(0), Row::Raster(low), Row::Text(1), Row::Raster(high)];
    assert_eq!(composite_rows(&stack, &texts, &bands), expected);
    assert_eq!(tree_rows(&stack, &texts, &bands), expected);
}

/// B3: equal-Z items (here all unbanded) list rasters below texts and texts in creation order —
/// never by page-Y — in the panel exactly as on the canvas.
#[test]
fn tree_matches_composite_for_equal_z_ties() {
    let plain = stack();
    let mut with_raster = stack();
    let raster = with_raster.add_raster_layer();
    // Page-Y would order these 2, 1, 0 (and the negative one below the raster).
    let texts = [
        text_layer("first", 0, true, 50.0),
        text_layer("second", 0, true, 10.0),
        text_layer("third", 0, true, -5.0),
    ];
    let expected = vec![Row::Raster(raster), Row::Text(0), Row::Text(1), Row::Text(2)];
    assert_eq!(composite_rows(&with_raster, &texts, &[]), expected);
    assert_eq!(tree_rows(&with_raster, &texts, &[]), expected);

    // Same for unpinned members of one legacy `TextGroup` band.
    let bands = vec![Band::TextGroup {
        layer_idx: 0,
        z: 0,
        member_uids: Vec::new(),
    }];
    let grouped = [
        text_layer("a", 0, false, 30.0),
        text_layer("b", 0, false, 20.0),
    ];
    assert_eq!(composite_rows(&plain, &grouped, &bands), vec![Row::Text(0), Row::Text(1)]);
    assert_eq!(tree_rows(&plain, &grouped, &bands), vec![Row::Text(0), Row::Text(1)]);
}

/// The panel lists hidden rows too; restricted to what the canvas draws, its order is the plan's.
#[test]
fn tree_lists_hidden_rows_in_composite_order() {
    let mut stack = stack();
    let shown = stack.add_raster_layer();
    let hidden = stack.add_raster_layer();
    let group = stack.add_group("hidden".to_owned());
    stack.set_layer_group(hidden, Some(group));
    if let Some(g) = stack.group_mut(group) {
        g.visible = false;
    }
    let mut texts = [text_layer("t", 0, true, 0.0)];
    texts[0].visible = false;

    let tree = tree_rows(&stack, &texts, &[]);
    assert_eq!(tree, vec![Row::Raster(shown), Row::Raster(hidden), Row::Text(0)]);
    let drawn = composite_rows(&stack, &texts, &[]);
    assert_eq!(drawn, vec![Row::Raster(shown)]);
    let tree_drawn: Vec<Row> = tree.into_iter().filter(|row| drawn.contains(row)).collect();
    assert_eq!(tree_drawn, drawn);
}

// ---- structural band order == panel rows; ▲▼ acts on the visible neighbour ----

/// The current group membership, as `PsEditorTabState::current_membership` builds it.
fn membership(stack: &LayerStack, texts: &[PsTextLayer]) -> HashMap<String, Option<String>> {
    let mut group_of: HashMap<String, Option<String>> = stack
        .layers()
        .iter()
        .filter(|layer| !layer.kind.is_base())
        .map(|layer| (layer.uid.to_string(), stack.layer_group_uid(layer.id)))
        .collect();
    for text in texts {
        group_of.insert(text.uid.clone(), text.group_uid.clone());
    }
    group_of
}

/// The band a panel row stands for (every text is its own `PinnedText` band).
fn band_of(row: Row, stack: &LayerStack, texts: &[PsTextLayer]) -> Option<persist::BandRef> {
    match row {
        Row::Raster(id) => stack
            .layer(id)
            .map(|l| persist::BandRef::Raster(l.uid.to_string())),
        Row::Text(index) => texts
            .get(index)
            .map(|t| persist::BandRef::PinnedText(t.uid.clone())),
    }
}

/// The structural order the move ops start from, without the group column.
fn structural_order(stack: &LayerStack, texts: &[PsTextLayer], bands: &[Band]) -> Vec<persist::BandRef> {
    tree::unified_band_order(stack, texts, bands, &membership(stack, texts))
        .into_iter()
        .map(|(band, _)| band)
        .collect()
}

/// The panel rows as bands, bottom-to-top.
fn tree_bands(stack: &LayerStack, texts: &[PsTextLayer], bands: &[Band]) -> Vec<persist::BandRef> {
    tree_rows(stack, texts, bands)
        .into_iter()
        .filter_map(|row| band_of(row, stack, texts))
        .collect()
}

/// `build_unified_order` (via `tree::unified_band_order`) lists exactly the panel's rows, in the
/// panel's order, for the B3 / B4 fixtures and with a contiguous group.
#[test]
fn structural_order_agrees_with_tree_rows() {
    // B4: unpinned doc text at its node Z.
    let mut stack = stack();
    let low = stack.add_raster_layer();
    let high = stack.add_raster_layer();
    let uid = |id: LayerId, stack: &LayerStack| stack.layer(id).map(|l| l.uid.to_string()).unwrap_or_default();
    let page = DocPage {
        nodes: vec![
            doc_node("pinned", NodeKind::Text, 0, true),
            doc_node(&uid(low, &stack), NodeKind::Raster, 1, false),
            doc_node("unpinned", NodeKind::Text, 2, false),
            doc_node(&uid(high, &stack), NodeKind::Raster, 3, false),
        ],
        groups: Vec::new(),
    };
    let bands = ordering::doc_page_bands(&page);
    let texts = [text_layer("pinned", 0, true, 0.0), text_layer("unpinned", 0, false, 0.0)];
    assert_eq!(structural_order(&stack, &texts, &bands), tree_bands(&stack, &texts, &bands));

    // B3: equal-Z (unbanded) ties.
    let ties = [text_layer("a", 0, true, 50.0), text_layer("b", 0, true, -5.0)];
    assert_eq!(structural_order(&stack, &ties, &[]), tree_bands(&stack, &ties, &[]));

    // A contiguous group (raster + text) keeps the panel's order too.
    let group = stack.add_group("g".to_owned());
    stack.set_layer_group(high, Some(group));
    let group_uid = stack.group(group).map(|g| g.uid.to_string());
    let mut grouped = [text_layer("pinned", 0, true, 0.0), text_layer("unpinned", 0, false, 0.0)];
    grouped[1].group_uid = group_uid;
    assert_eq!(structural_order(&stack, &grouped, &bands), tree_bands(&stack, &grouped, &bands));
}

/// ▲▼ next to equal-Z texts swaps with the VISUALLY adjacent row (input order), not the page-Y one.
#[test]
fn move_next_to_equal_z_texts_swaps_with_visible_neighbour() {
    let mut stack = stack();
    let raster = stack.add_raster_layer();
    let raster_uid = stack.layer(raster).map(|l| l.uid.to_string()).unwrap_or_default();
    // Unbanded: all at Z 0, so the panel shows raster, first, second (page-Y would flip the texts).
    let texts = [text_layer("first", 0, true, 50.0), text_layer("second", 0, true, 10.0)];
    let r = persist::BandRef::Raster(raster_uid);
    let first = persist::BandRef::PinnedText("first".to_owned());
    let second = persist::BandRef::PinnedText("second".to_owned());
    let order = || tree::unified_band_order(&stack, &texts, &[], &membership(&stack, &texts));
    assert_eq!(tree_bands(&stack, &texts, &[]), vec![r.clone(), first.clone(), second.clone()]);

    let up = band_order_after_move(order(), &r, true);
    assert_eq!(up, Some((vec![first.clone(), r.clone(), second.clone()], false)));
    let down = band_order_after_move(order(), &second, false);
    assert_eq!(down, Some((vec![r.clone(), second.clone(), first.clone()], false)));
    assert_eq!(band_order_after_move(order(), &second, true), None, "already on top");
}

/// ▲▼ next to an UNPINNED doc text treats it as its own row at its node Z, and the unpinned text
/// itself can be moved (the move persists it as a `PinnedText` band at the new Z).
#[test]
fn move_next_to_unpinned_text_swaps_with_visible_neighbour() {
    let mut stack = stack();
    let low = stack.add_raster_layer();
    let high = stack.add_raster_layer();
    let uid = |id: LayerId, stack: &LayerStack| stack.layer(id).map(|l| l.uid.to_string()).unwrap_or_default();
    let (low_uid, high_uid) = (uid(low, &stack), uid(high, &stack));
    let page = DocPage {
        nodes: vec![
            doc_node("pinned", NodeKind::Text, 0, true),
            doc_node(&low_uid, NodeKind::Raster, 1, false),
            doc_node("unpinned", NodeKind::Text, 2, false),
            doc_node(&high_uid, NodeKind::Raster, 3, false),
        ],
        groups: Vec::new(),
    };
    let bands = ordering::doc_page_bands(&page);
    let texts = [text_layer("pinned", 0, true, 0.0), text_layer("unpinned", 0, false, 0.0)];
    let order = || tree::unified_band_order(&stack, &texts, &bands, &membership(&stack, &texts));
    let pinned = persist::BandRef::PinnedText("pinned".to_owned());
    let unpinned = persist::BandRef::PinnedText("unpinned".to_owned());
    let r_low = persist::BandRef::Raster(low_uid);
    let r_high = persist::BandRef::Raster(high_uid);

    // The top raster moves down past the unpinned text, which the panel shows directly below it.
    assert_eq!(
        band_order_after_move(order(), &r_high, false),
        Some((vec![pinned.clone(), r_low.clone(), r_high.clone(), unpinned.clone()], false))
    );
    // The unpinned text moves down past the raster below it.
    assert_eq!(
        band_order_after_move(order(), &unpinned, false),
        Some((vec![pinned.clone(), unpinned.clone(), r_low.clone(), r_high.clone()], false))
    );
}

/// A grouped row swaps only with its adjacent row inside the group's run.
#[test]
fn grouped_move_stays_inside_its_run() {
    let mut stack = stack();
    let a = stack.add_raster_layer();
    let b = stack.add_raster_layer();
    let outside = stack.add_raster_layer();
    let group = stack.add_group("g".to_owned());
    stack.set_layer_group(a, Some(group));
    stack.set_layer_group(b, Some(group));
    let refs: Vec<persist::BandRef> = [a, b, outside]
        .iter()
        .filter_map(|id| stack.layer(*id).map(|l| persist::BandRef::Raster(l.uid.to_string())))
        .collect();
    let order = tree::unified_band_order(&stack, &[], &[], &membership(&stack, &[]));
    assert_eq!(
        band_order_after_move(order.clone(), &refs[0], true),
        Some((vec![refs[1].clone(), refs[0].clone(), refs[2].clone()], true))
    );
    assert_eq!(band_order_after_move(order, &refs[1], true), None, "top of its run");
}
