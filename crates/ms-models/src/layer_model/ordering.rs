/*
File: models/layer_model/ordering.rs

Purpose:
The ONE owner of layer composite order, visibility and group fold for a page (GUI-free, no I/O).
Both the typing tab (canvas draw, CPU flatten / export) and the PS editor (GPU composite) derive
their bottom-to-top draw list from here, so the two tabs and the export cannot disagree.

Bands: the unified bottom-to-top Z axis of a page as a list of *bands*. Text is FULLY MANUAL now:
every text node is pinned-with-explicit-Z and forms its own band, interleaved with rasters on one Z
axis (text may sit BELOW a raster). The legacy `TextGroup` band (Группа текста N, auto-sub-ordered by
page-Y) is RETIRED for new data — `page_bands` still emits it for a not-yet-resaved legacy chapter,
but `layer_doc::ensure_page_loaded` FLATTENS each such group into per-text bands ON READ (preserving
the current page-Y visual order), and the next save dissolves the group into pinned bands. Band Z is
explicit in the manifest / doc node; this module only looks it up and sorts by it.

Key structures:
- Band — one band on the unified Z axis.
- CompositeKind / CompositeKey / CompositeRank — what an item is, how it finds its band, and its
  sort key (Raster below Text at equal Z).
- CompositeItem / GroupFold / CompositeStep — the composite-plan input and output.

Key functions:
- page_bands() — manifest -> bands sorted by Z (disk paths).
- doc_page_bands() — resident `DocPage` -> bands (in-memory paths).
- band_z() — the uid-first band-Z lookup (missing item => top of stack).
- composite_rank() — the (Z, kind) sort key, also used by hit-tests.
- composite_plan() — visibility + group fold + stable bottom-to-top order.

Notes:
Ties at equal rank resolve by INPUT order (stable sort): callers pass rasters in stack order and
texts in creation order (newest last = on top). The legacy page-Y tie-break is not reproduced.
*/

use super::layer_doc::{DocPage, NodeKind};
use super::manifest::{LayerKindRec, PageLayers};
use super::persist::GroupMeta;

/// One band on the unified Z axis. Some fields are consumed by the per-tab renderers (next step).
#[derive(Debug, Clone)]
pub enum Band {
    /// A raster layer node.
    Raster { uid: String, z: u32 },
    /// A text group: the unpinned text overlays sharing `layer_idx`. `member_uids` is unordered —
    /// the caller sorts them by page-Y (lower on the page = higher in the stack), like the legacy
    /// `overlay_stack_cmp`.
    TextGroup {
        layer_idx: u32,
        z: u32,
        member_uids: Vec<String>,
    },
    /// A single pinned text overlay at an explicit Z (no auto page-Y ordering).
    PinnedText { uid: String, z: u32 },
}

impl Band {
    #[must_use]
    pub fn z(&self) -> u32 {
        match self {
            Band::Raster { z, .. } | Band::TextGroup { z, .. } | Band::PinnedText { z, .. } => *z,
        }
    }

    /// The reorder reference (uid / layer_idx) for this band, for `persist::save_page_band_order`.
    #[must_use]
    pub fn to_ref(&self) -> super::persist::BandRef {
        match self {
            Band::Raster { uid, .. } => super::persist::BandRef::Raster(uid.clone()),
            Band::TextGroup { layer_idx, .. } => super::persist::BandRef::TextGroup(*layer_idx),
            Band::PinnedText { uid, .. } => super::persist::BandRef::PinnedText(uid.clone()),
        }
    }
}

/// Returns the page's bands sorted bottom-to-top by unified Z.
#[must_use]
pub fn page_bands(page: &PageLayers) -> Vec<Band> {
    let mut bands = Vec::new();

    for node in &page.tree {
        match node.kind {
            LayerKindRec::Raster => bands.push(Band::Raster {
                uid: node.uid.clone(),
                z: node.z,
            }),
            LayerKindRec::Text if node.pinned => bands.push(Band::PinnedText {
                uid: node.uid.clone(),
                z: node.z,
            }),
            // Unpinned text emits no per-node band here — it is collected into a `TextGroup` band by
            // the `page.text_groups` pass below. A `Group` is a container, not a Z band. Both are
            // exhaustive no-ops (a new `LayerKindRec` variant must be reconsidered here, not silently
            // dropped — CLAUDE.md §17).
            LayerKindRec::Text => {}
            LayerKindRec::Group => {}
        }
    }

    for group in &page.text_groups {
        let member_uids: Vec<String> = page
            .tree
            .iter()
            .filter(|r| {
                r.kind == LayerKindRec::Text && !r.pinned && r.layer_idx == Some(group.layer_idx)
            })
            .map(|r| r.uid.clone())
            .collect();
        bands.push(Band::TextGroup {
            layer_idx: group.layer_idx,
            z: group.z,
            member_uids,
        });
    }

    bands.sort_by_key(Band::z);
    bands
}

/// Returns the bands of a resident doc page: one `Raster` band per raster node and one `PinnedText`
/// band per text node (every doc text node is pinned-with-explicit-Z), each at its node `z`.
///
/// Bands come out in node order, which is bottom-to-top by the `DocPage` invariant (nodes sorted by
/// unique `z`); no `TextGroup` band is ever emitted, because the doc has no group-band concept.
#[must_use]
pub fn doc_page_bands(page: &DocPage) -> Vec<Band> {
    page.nodes
        .iter()
        .map(|node| match node.kind {
            NodeKind::Raster => Band::Raster {
                uid: node.uid.clone(),
                z: node.z,
            },
            NodeKind::Text => Band::PinnedText {
                uid: node.uid.clone(),
                z: node.z,
            },
        })
        .collect()
}

/// What a composite item is. At equal band Z a `Raster` sorts BELOW a `Text` (text on top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeKind {
    Raster,
    Text,
}

/// How a composite item finds its band: a raster by its node uid; a text by its node uid first and,
/// for a legacy chapter whose bands still hold a `TextGroup`, by its «Группа текста N» `layer_idx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeKey<'a> {
    Raster { uid: &'a str },
    Text { uid: &'a str, layer_idx: u32 },
}

impl CompositeKey<'_> {
    /// The kind this key addresses.
    #[must_use]
    pub fn kind(&self) -> CompositeKind {
        match self {
            CompositeKey::Raster { .. } => CompositeKind::Raster,
            CompositeKey::Text { .. } => CompositeKind::Text,
        }
    }
}

/// Bottom-to-top sort key of a composite item: band Z first, then kind (`Raster` < `Text`).
/// A greater rank draws later (on top) and wins a hit-test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompositeRank {
    z: u32,
    kind_rank: u8,
}

impl CompositeRank {
    /// The band Z this rank was built from.
    #[must_use]
    pub fn z(self) -> u32 {
        self.z
    }
}

/// One item the caller wants composited (a raster layer or a text / image overlay).
///
/// `visible` / `opacity` are the item's OWN values, before any group fold; `group_uid` is its PS
/// unified group (`LayerNode::group_uid`), orthogonal to a text's `layer_idx`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositeItem<'a> {
    pub key: CompositeKey<'a>,
    pub group_uid: Option<&'a str>,
    pub visible: bool,
    pub opacity: f32,
}

/// Borrowed view of one PS unified group, as far as compositing is concerned. Nesting
/// (`GroupRec::parent_uid`) is reserved in the data model and deliberately not folded here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupFold<'a> {
    pub uid: &'a str,
    pub visible: bool,
    pub opacity: f32,
}

impl<'a> From<&'a GroupMeta> for GroupFold<'a> {
    fn from(meta: &'a GroupMeta) -> Self {
        GroupFold {
            uid: &meta.uid,
            visible: meta.visible,
            opacity: meta.opacity,
        }
    }
}

/// One step of a composite plan: draw `items[item]` at band `z` with the folded `opacity`
/// (always finite and in `(0, 1]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositeStep {
    pub item: usize,
    pub z: u32,
    pub opacity: f32,
}

/// The band Z of an item (uid-first rule): a raster takes the Z of the `Raster` band with its uid; a
/// text takes the Z of the `PinnedText` band with its uid, else of the `TextGroup` band with its
/// `layer_idx`. An item with no band (not yet in the manifest) gets the top-of-stack key
/// `bands.len()` (so `0` for empty bands). The first matching band wins.
#[must_use]
pub fn band_z(bands: &[Band], key: CompositeKey<'_>) -> u32 {
    let found = match key {
        CompositeKey::Raster { uid } => bands.iter().find_map(|band| match band {
            Band::Raster { uid: u, z } if u == uid => Some(*z),
            _ => None,
        }),
        CompositeKey::Text { uid, layer_idx } => bands
            .iter()
            .find_map(|band| match band {
                Band::PinnedText { uid: u, z } if u == uid => Some(*z),
                _ => None,
            })
            .or_else(|| {
                bands.iter().find_map(|band| match band {
                    Band::TextGroup {
                        layer_idx: li, z, ..
                    } if *li == layer_idx => Some(*z),
                    _ => None,
                })
            }),
    };
    // A page cannot hold u32::MAX bands; saturating keeps the "above every band" meaning anyway.
    found.unwrap_or_else(|| u32::try_from(bands.len()).unwrap_or(u32::MAX))
}

/// The composite sort key of an item of `kind` at band `z` (see [`CompositeRank`]).
#[must_use]
pub fn composite_rank(z: u32, kind: CompositeKind) -> CompositeRank {
    let kind_rank = match kind {
        CompositeKind::Raster => 0,
        CompositeKind::Text => 1,
    };
    CompositeRank { z, kind_rank }
}

/// The bottom-to-top draw plan of `items` on a page with `bands` and PS unified `groups`.
///
/// An item is OMITTED when it is not `visible`, when its group exists and is hidden, or when its
/// effective opacity `clamp(item.opacity, 0, 1) * clamp(group.opacity, 0, 1)` is `<= 0` or NaN
/// (never a panic; the caller owns the inputs and logs a NaN it passed). A `group_uid` naming no
/// group in `groups` folds as visible with opacity 1. The kept items are ordered by a stable sort on
/// `(composite_rank(band_z, kind), input index)`, so callers pass rasters in stack order and texts
/// in creation order (newest last = on top). Cost is O(items × (bands + groups)): pages hold tens of
/// layers, so no index is built.
#[must_use]
pub fn composite_plan(bands: &[Band], groups: &[GroupFold<'_>], items: &[CompositeItem<'_>]) -> Vec<CompositeStep> {
    let mut ranked: Vec<(CompositeRank, CompositeStep)> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if !item.visible {
            continue;
        }
        let group = item
            .group_uid
            .and_then(|uid| groups.iter().find(|g| g.uid == uid));
        let group_opacity = match group {
            Some(g) if !g.visible => continue,
            Some(g) => g.opacity,
            None => 1.0,
        };
        // `f32::clamp` passes NaN through, so the explicit NaN test below still sees it.
        let opacity = item.opacity.clamp(0.0, 1.0) * group_opacity.clamp(0.0, 1.0);
        if opacity.is_nan() || opacity <= 0.0 {
            continue;
        }
        let z = band_z(bands, item.key);
        ranked.push((
            composite_rank(z, item.key.kind()),
            CompositeStep {
                item: index,
                z,
                opacity,
            },
        ));
    }
    // `ranked` is in input order and `sort_by_key` is stable, so equal ranks keep input order.
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, step)| step).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer_model::manifest::{LayerRec, TextGroupRec, TransformRec};

    fn raster(uid: &str, z: u32) -> LayerRec {
        LayerRec {
            uid: uid.into(),
            name: uid.into(),
            kind: LayerKindRec::Raster,
            z,
            layer_idx: None,
            pinned: false,
            pinned_by_group: false,
            group_uid: None,
            visible: true,
            opacity: 1.0,
            transform: Some(TransformRec {
                cx: 0.0,
                cy: 0.0,
                rotation: 0.0,
                scale: 1.0,
            }),
            deform: None,
            base_file: Some(format!("{uid}.png")),
            rendered_file: None,
            image_size: Some([1, 1]),
            effects: Vec::new(),
            payload_ref: None,
            render_data: None,
            overlay_is_image: None,
            mask_clip: None,
            text_centers: None,
            centering_frame: None,
        }
    }

    fn text(uid: &str, layer_idx: u32, pinned: bool, z: u32) -> LayerRec {
        LayerRec {
            uid: uid.into(),
            name: uid.into(),
            kind: LayerKindRec::Text,
            z,
            layer_idx: Some(layer_idx),
            pinned,
            pinned_by_group: false,
            group_uid: None,
            visible: true,
            opacity: 1.0,
            transform: None,
            deform: None,
            base_file: None,
            rendered_file: None,
            image_size: None,
            effects: Vec::new(),
            payload_ref: None,
            render_data: None,
            overlay_is_image: None,
            mask_clip: None,
            text_centers: None,
            centering_frame: None,
        }
    }

    #[test]
    fn bands_interleave_rasters_groups_and_pinned_by_z() {
        let page = PageLayers {
            img_idx: 0,
            groups: Vec::new(),
            // Raster r0 at z=0, text group 0 band at z=1, raster r1 at z=2, group 1 at z=3,
            // a pinned text at z=4 (top).
            text_groups: vec![
                TextGroupRec {
                    layer_idx: 0,
                    z: 1,
                    name: "g0".into(),
                },
                TextGroupRec {
                    layer_idx: 1,
                    z: 3,
                    name: "g1".into(),
                },
            ],
            tree: vec![
                raster("r0", 0),
                raster("r1", 2),
                text("t_a", 0, false, 0),
                text("t_b", 0, false, 0),  // both in group 0
                text("t_c", 1, false, 0),  // group 1
                text("t_pin", 0, true, 4), // pinned, its own band at top
            ],
        };

        let bands = page_bands(&page);
        let zs: Vec<u32> = bands.iter().map(Band::z).collect();
        assert_eq!(zs, vec![0, 1, 2, 3, 4], "bands sorted by unified z");

        // Band at z=1 is text group 0 with both unpinned members.
        match &bands[1] {
            Band::TextGroup {
                layer_idx,
                member_uids,
                ..
            } => {
                assert_eq!(*layer_idx, 0);
                assert_eq!(member_uids.len(), 2);
                assert!(member_uids.contains(&"t_a".to_string()));
                assert!(member_uids.contains(&"t_b".to_string()));
            }
            other => panic!("expected text group at z=1, got {other:?}"),
        }
        // Top band is the pinned text.
        assert!(matches!(&bands[4], Band::PinnedText { uid, .. } if uid == "t_pin"));
    }

    // ---- composite owner: doc bands, band_z, rank, plan (site table rows 2-5) ----

    use crate::layer_model::layer_doc::{LayerNode, NodeBody};
    use eframe::egui::{Color32, ColorImage};

    fn node(uid: &str, kind: NodeKind, z: u32) -> LayerNode {
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
                payload_uid: uid.into(),
                mask_clip: None,
                extra_centers: ms_text_render::types::RenderedTextExtraInfo::default(),
                centering_frame: None,
            },
        };
        LayerNode {
            uid: uid.into(),
            name: uid.into(),
            kind,
            z,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            text_layer_idx: matches!(kind, NodeKind::Text).then_some(0),
            text_pinned: matches!(kind, NodeKind::Text),
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

    fn raster_band(uid: &str, z: u32) -> Band {
        Band::Raster { uid: uid.into(), z }
    }

    fn pinned_band(uid: &str, z: u32) -> Band {
        Band::PinnedText { uid: uid.into(), z }
    }

    fn group_band(layer_idx: u32, z: u32) -> Band {
        Band::TextGroup {
            layer_idx,
            z,
            member_uids: Vec::new(),
        }
    }

    fn r_item(uid: &str) -> CompositeItem<'_> {
        CompositeItem {
            key: CompositeKey::Raster { uid },
            group_uid: None,
            visible: true,
            opacity: 1.0,
        }
    }

    fn t_item(uid: &str) -> CompositeItem<'_> {
        CompositeItem {
            key: CompositeKey::Text { uid, layer_idx: 0 },
            group_uid: None,
            visible: true,
            opacity: 1.0,
        }
    }

    fn order(steps: &[CompositeStep]) -> Vec<usize> {
        steps.iter().map(|s| s.item).collect()
    }

    // Row 2: raster -> Raster, text -> PinnedText, z = node z, node order kept.
    #[test]
    fn doc_page_bands_maps_every_node_in_node_order() {
        let page = DocPage {
            nodes: vec![
                node("r0", NodeKind::Raster, 0),
                node("t0", NodeKind::Text, 1),
                node("r1", NodeKind::Raster, 2),
            ],
            groups: Vec::new(),
        };
        let bands = doc_page_bands(&page);
        assert_eq!(bands.len(), 3);
        assert!(matches!(&bands[0], Band::Raster { uid, z: 0 } if uid == "r0"));
        assert!(matches!(&bands[1], Band::PinnedText { uid, z: 1 } if uid == "t0"));
        assert!(matches!(&bands[2], Band::Raster { uid, z: 2 } if uid == "r1"));
        assert!(doc_page_bands(&DocPage { nodes: Vec::new(), groups: Vec::new() }).is_empty());
    }

    // Row 3: raster by uid; unknown => bands.len(); empty bands => 0.
    #[test]
    fn band_z_raster_by_uid_missing_is_top() {
        let bands = vec![raster_band("r0", 0), pinned_band("t0", 1), raster_band("r1", 2)];
        assert_eq!(band_z(&bands, CompositeKey::Raster { uid: "r1" }), 2);
        assert_eq!(band_z(&bands, CompositeKey::Raster { uid: "new" }), 3);
        assert_eq!(band_z(&[], CompositeKey::Raster { uid: "r0" }), 0);
        // A raster key never matches a text band carrying the same uid.
        assert_eq!(band_z(&bands, CompositeKey::Raster { uid: "t0" }), 3);
    }

    // Row 3: text PinnedText by uid first, else TextGroup by layer_idx, else bands.len().
    #[test]
    fn band_z_text_is_uid_first_then_layer_idx() {
        let bands = vec![group_band(0, 0), raster_band("r0", 1), pinned_band("t_pin", 2), group_band(1, 3)];
        // uid wins over a matching TextGroup.
        assert_eq!(band_z(&bands, CompositeKey::Text { uid: "t_pin", layer_idx: 1 }), 2);
        // no PinnedText band => its layer_idx group.
        assert_eq!(band_z(&bands, CompositeKey::Text { uid: "t_a", layer_idx: 0 }), 0);
        assert_eq!(band_z(&bands, CompositeKey::Text { uid: "t_b", layer_idx: 1 }), 3);
        // neither => top of stack.
        assert_eq!(band_z(&bands, CompositeKey::Text { uid: "t_c", layer_idx: 9 }), 4);
        assert_eq!(band_z(&[], CompositeKey::Text { uid: "t_c", layer_idx: 0 }), 0);
        // A text key never matches a raster band carrying the same uid.
        assert_eq!(band_z(&bands, CompositeKey::Text { uid: "r0", layer_idx: 9 }), 4);
    }

    // Rows 4/6: Z dominates, then Raster < Text at equal Z.
    #[test]
    fn composite_rank_orders_z_then_raster_below_text() {
        assert!(composite_rank(1, CompositeKind::Raster) < composite_rank(1, CompositeKind::Text));
        assert!(composite_rank(1, CompositeKind::Text) < composite_rank(2, CompositeKind::Raster));
        assert_eq!(composite_rank(7, CompositeKind::Text).z(), 7);
    }

    // Row 4: interleave by band Z (text may sit below a raster); missing band goes on top.
    #[test]
    fn plan_orders_by_band_z_with_missing_on_top() {
        let bands = vec![raster_band("r0", 0), pinned_band("t0", 1), raster_band("r1", 2)];
        let items = [t_item("t_new"), r_item("r1"), t_item("t0"), r_item("r0")];
        let steps = composite_plan(&bands, &[], &items);
        assert_eq!(order(&steps), vec![3, 2, 1, 0]);
        assert_eq!(steps.iter().map(|s| s.z).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        assert!(steps.iter().all(|s| s.opacity == 1.0));
    }

    // Row 4: equal Z => raster below text, regardless of input order.
    #[test]
    fn plan_same_z_puts_raster_below_text() {
        let bands = vec![raster_band("r0", 0), pinned_band("t0", 0)];
        let steps = composite_plan(&bands, &[], &[t_item("t0"), r_item("r0")]);
        assert_eq!(order(&steps), vec![1, 0]);
    }

    // Row 4 / B3: equal rank resolves by input order (newest last = on top), never page-Y.
    #[test]
    fn plan_equal_rank_keeps_input_order() {
        let bands = vec![raster_band("r0", 0)];
        // Three texts not yet in the manifest share z = bands.len(); two unknown rasters too.
        let items = [t_item("a"), r_item("x"), t_item("b"), r_item("y"), t_item("c")];
        let steps = composite_plan(&bands, &[], &items);
        assert_eq!(order(&steps), vec![1, 3, 0, 2, 4]);
        // Empty bands: every item at z = 0, still rasters first then texts, each in input order.
        let steps = composite_plan(&[], &[], &items);
        assert_eq!(order(&steps), vec![1, 3, 0, 2, 4]);
        assert!(steps.iter().all(|s| s.z == 0));
    }

    #[test]
    fn plan_empty_input_is_empty() {
        assert!(composite_plan(&[], &[], &[]).is_empty());
        let bands = vec![raster_band("r0", 0)];
        let groups = [GroupFold { uid: "g", visible: true, opacity: 1.0 }];
        assert!(composite_plan(&bands, &groups, &[]).is_empty());
    }

    // Item visibility and own opacity.
    #[test]
    fn plan_omits_invisible_and_transparent_items_and_clamps_opacity() {
        let mut hidden = r_item("r0");
        hidden.visible = false;
        let mut zero = t_item("t0");
        zero.opacity = 0.0;
        let mut negative = r_item("r1");
        negative.opacity = -0.5;
        let mut over = t_item("t1");
        over.opacity = 1.7;
        let mut half = r_item("r2");
        half.opacity = 0.5;
        let steps = composite_plan(&[], &[], &[hidden, zero, negative, over, half]);
        assert_eq!(order(&steps), vec![4, 3]);
        assert_eq!(steps[0].opacity, 0.5);
        assert_eq!(steps[1].opacity, 1.0);
    }

    // NaN opacity (item or group) is omitted, never a panic.
    #[test]
    fn plan_omits_nan_opacity() {
        let mut nan_item = r_item("r0");
        nan_item.opacity = f32::NAN;
        let mut in_nan_group = t_item("t0");
        in_nan_group.group_uid = Some("g");
        let groups = [GroupFold { uid: "g", visible: true, opacity: f32::NAN }];
        let steps = composite_plan(&[], &groups, &[nan_item, in_nan_group, r_item("r1")]);
        assert_eq!(order(&steps), vec![2]);
    }

    // Row 5 / B1+B2: a hidden group omits its rasters AND texts; a dimmed group multiplies.
    #[test]
    fn plan_folds_group_visibility_and_opacity() {
        let groups = [
            GroupFold { uid: "hidden", visible: false, opacity: 1.0 },
            GroupFold { uid: "dim", visible: true, opacity: 0.5 },
            GroupFold { uid: "clear", visible: true, opacity: 0.0 },
        ];
        let mut r_hidden = r_item("r0");
        r_hidden.group_uid = Some("hidden");
        let mut t_hidden = t_item("t0");
        t_hidden.group_uid = Some("hidden");
        let mut r_dim = r_item("r1");
        r_dim.group_uid = Some("dim");
        r_dim.opacity = 0.5;
        let mut t_dim = t_item("t1");
        t_dim.group_uid = Some("dim");
        let mut t_clear = t_item("t2");
        t_clear.group_uid = Some("clear");
        let steps = composite_plan(&[], &groups, &[r_hidden, t_hidden, r_dim, t_dim, t_clear]);
        assert_eq!(order(&steps), vec![2, 3]);
        assert_eq!(steps[0].opacity, 0.25);
        assert_eq!(steps[1].opacity, 0.5);
    }

    // Row 5: an unknown group uid folds as visible / 1.0; GroupMeta converts into a fold view.
    #[test]
    fn plan_unknown_group_folds_visible_full_opacity() {
        let meta = GroupMeta {
            uid: "g".into(),
            name: "G".into(),
            visible: true,
            opacity: 0.25,
            collapsed: false,
        };
        let groups = [GroupFold::from(&meta)];
        let mut orphan = r_item("r0");
        orphan.group_uid = Some("gone");
        orphan.opacity = 0.75;
        let mut member = t_item("t0");
        member.group_uid = Some("g");
        let steps = composite_plan(&[], &groups, &[orphan, member]);
        assert_eq!(order(&steps), vec![0, 1]);
        assert_eq!(steps[0].opacity, 0.75);
        assert_eq!(steps[1].opacity, 0.25);
    }

    // Row 3 on legacy disk bands: an unpinned text finds its TextGroup band through the plan.
    #[test]
    fn plan_uses_text_group_band_for_legacy_text() {
        let bands = vec![group_band(0, 0), raster_band("r0", 1)];
        let steps = composite_plan(&bands, &[], &[r_item("r0"), t_item("legacy")]);
        assert_eq!(order(&steps), vec![1, 0]);
        assert_eq!(steps[0].z, 0);
    }
}
