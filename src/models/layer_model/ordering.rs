/*
File: models/layer_model/ordering.rs

Purpose:
Computes the unified bottom-to-top Z order of a page as a list of *bands*, shared by both tabs so
they render layers in the same order. Text is FULLY MANUAL now: every text node is pinned-with-explicit-Z
and forms its own band, interleaved with rasters on one Z axis (text may sit BELOW a raster). The legacy
`TextGroup` band (Группа текста N, auto-sub-ordered by page-Y) is RETIRED for new data — `page_bands`
still emits it for a not-yet-resaved legacy chapter, but `layer_doc::ensure_page_loaded` FLATTENS each
such group into per-text bands ON READ (preserving the current page-Y visual order), and the next save
dissolves the group into pinned bands. Band Z is explicit in the manifest; this module just sorts by it.
*/

use super::manifest::{LayerKindRec, PageLayers};

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
            _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::layer_model::manifest::{LayerRec, TextGroupRec, TransformRec};

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
            transform: Some(TransformRec { cx: 0.0, cy: 0.0, rotation: 0.0, scale: 1.0 }),
            deform: None,
            base_file: Some(format!("{uid}.png")),
            rendered_file: None,
            image_size: Some([1, 1]),
            effects: Vec::new(),
            payload_ref: None,
            render_data: None,
            mask_clip: None,
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
            mask_clip: None,
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
                TextGroupRec { layer_idx: 0, z: 1, name: "g0".into() },
                TextGroupRec { layer_idx: 1, z: 3, name: "g1".into() },
            ],
            tree: vec![
                raster("r0", 0),
                raster("r1", 2),
                text("t_a", 0, false, 0),
                text("t_b", 0, false, 0), // both in group 0
                text("t_c", 1, false, 0), // group 1
                text("t_pin", 0, true, 4), // pinned, its own band at top
            ],
        };

        let bands = page_bands(&page);
        let zs: Vec<u32> = bands.iter().map(Band::z).collect();
        assert_eq!(zs, vec![0, 1, 2, 3, 4], "bands sorted by unified z");

        // Band at z=1 is text group 0 with both unpinned members.
        match &bands[1] {
            Band::TextGroup { layer_idx, member_uids, .. } => {
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
}
