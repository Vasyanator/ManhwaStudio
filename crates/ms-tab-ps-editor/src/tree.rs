/*
File: tabs/ps_editor/tree.rs

Purpose:
Builds the unified, Photoshop-like layer tree shown in the PS editor's layers panel. It is a pure
*view* derived each frame from the editor's existing stores — it owns no state and persists nothing.

The tree joins the three otherwise-disjoint stores into one hierarchy ordered by the unified Z axis:
- raster layers (`LayerStack`, incl. the locked Source/Clean base layers),
- typing text overlays (`PsTextLayer`),
- and groups (`LayerGroup`, which may now mix rasters and texts).

A group renders as a collapsible header followed by its members indented one level. The crux
invariant (enforced at write time in `models::layer_model::persist::save_page_grouping`) is that a
group's members are contiguous on the Z axis, so the tree is just a flat sorted leaf list with
group runs bracketed by headers. The bottom-to-top leaf order here mirrors `draw_composite`'s plan
sort exactly (z, then rasters-below-texts, then text page-Y), so the panel order equals the
composite order.

The WHOLE emitted list is top-to-bottom, the base tail included: the base layers are appended last
(they are the bottom of the composite) but in REVERSE stack order, so `Клин` — which `draw_composite`
paints OVER `Исходник` — is listed above it. The `LayerStack` vector order itself is never changed:
it is load-bearing for compositing, and only this view is reordered.
*/

use super::layers::{LayerId, LayerStack};
use super::text_layers::PsTextLayer;
use ms_models::layer_model::ordering::Band;
use std::collections::HashMap;

/// One indentation step (px) per nesting level in the panel.
pub const INDENT: f32 = 16.0;

/// What a leaf row stands for.
#[derive(Debug, Clone)]
pub enum LeafKind {
    /// A base layer (Source / Clean): always the bottom two rows, never grouped or reordered.
    /// `Клин` is listed above `Исходник`, matching the composite.
    ///
    /// A base leaf IS selectable as the panel's primary row (`RowSel::Base`) — the editor always
    /// has an active layer and it defaults to `Клин`, so the row has to be able to show it. The
    /// structural lock is therefore stated per consumer, never by withholding the key: every
    /// structural consumer refuses `RowSel::Base` explicitly (`select_row` keeps it a solo
    /// primary and out of `panel_selection`, `selectable_row_order` keeps it out of the Shift range,
    /// `move_band_one` and `apply_group_op` return on it, and `draw_active_controls` gives it an arm
    /// with no destructive or reordering buttons). `Клин`'s pixels ARE editable, see
    /// `Layer::can_edit_pixels`.
    Base(LayerId),
    /// An editable raster layer.
    Raster(LayerId),
    /// A typing text overlay, by index into `text_layers`.
    Text(usize),
}

/// A single (non-group) row in the tree.
#[derive(Debug, Clone)]
pub struct Leaf {
    pub kind: LeafKind,
    /// Nesting depth (0 = top level, 1 = inside a group).
    pub depth: u8,
}

/// A collapsible group header row (snapshot of the group's metadata for the panel).
#[derive(Debug, Clone)]
pub struct GroupHeader {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub collapsed: bool,
    pub depth: u8,
}

/// A row in the rendered tree, top-to-bottom.
#[derive(Debug, Clone)]
pub enum TreeItem {
    Group(GroupHeader),
    Leaf(Leaf),
}

/// A flattened leaf with the keys needed to order it, before grouping into runs.
struct Flat {
    kind: LeafKind,
    group_uid: Option<String>,
    z: u32,
    /// Secondary sort key at equal Z: 0.0 for rasters (below texts), page-Y for texts. Mirrors the
    /// `draw_composite` tiebreak so panel order == composite order.
    secondary: f32,
}

/// Builds the unified tree top-to-bottom (first item renders highest). Group metadata (name /
/// visibility / opacity / collapse) is read from `stack.groups()`, which holds every group on the
/// page (including text-only ones, recreated on load from the manifest's `GroupRec`s).
///
/// Ordering contract: EVERY row of the returned list is top-to-bottom, including the two base
/// layers that close it. They are emitted in reverse stack order (`Клин` above `Исходник`) so the
/// panel matches what `draw_composite` paints; the stack vector is left untouched.
#[must_use]
pub fn build_unified_tree(
    stack: &LayerStack,
    text_layers: &[PsTextLayer],
    bands: &[Band],
) -> Vec<TreeItem> {
    // Band Z lookups (same maps `draw_composite` builds).
    let mut raster_z: HashMap<String, u32> = HashMap::new();
    let mut group_z: HashMap<u32, u32> = HashMap::new();
    let mut pinned_z: HashMap<String, u32> = HashMap::new();
    for band in bands {
        match band {
            Band::Raster { uid, z } => {
                raster_z.insert(uid.clone(), *z);
            }
            Band::TextGroup { layer_idx, z, .. } => {
                group_z.insert(*layer_idx, *z);
            }
            Band::PinnedText { uid, z } => {
                pinned_z.insert(uid.clone(), *z);
            }
        }
    }
    let top_z = bands.len() as u32;

    // Base layers first (the very bottom), then the band-ordered leaves above them.
    let mut base: Vec<Leaf> = Vec::new();
    let mut flat: Vec<Flat> = Vec::new();
    for layer in stack.layers() {
        if layer.kind.is_base() {
            base.push(Leaf {
                kind: LeafKind::Base(layer.id),
                depth: 0,
            });
            continue;
        }
        let z = raster_z
            .get(&layer.uid.to_string())
            .copied()
            .unwrap_or(top_z);
        flat.push(Flat {
            kind: LeafKind::Raster(layer.id),
            group_uid: stack.layer_group_uid(layer.id),
            z,
            secondary: 0.0,
        });
    }
    for (index, text) in text_layers.iter().enumerate() {
        let z = if text.pinned {
            pinned_z.get(&text.uid).copied()
        } else {
            group_z.get(&text.layer_idx).copied()
        }
        .unwrap_or(top_z);
        flat.push(Flat {
            kind: LeafKind::Text(index),
            group_uid: text.group_uid.clone(),
            z,
            secondary: text.center().y,
        });
    }

    // Bottom-to-top order, matching the composite plan sort.
    flat.sort_by(|a, b| a.z.cmp(&b.z).then(a.secondary.total_cmp(&b.secondary)));

    // Walk top-to-bottom (reverse), bracketing each maximal contiguous same-group run with a header.
    let mut out: Vec<TreeItem> = Vec::new();
    let mut seen_groups: Vec<String> = Vec::new();
    let mut i = flat.len();
    while i > 0 {
        i -= 1;
        let Some(uid) = flat[i].group_uid.clone() else {
            out.push(TreeItem::Leaf(Leaf {
                kind: flat[i].kind.clone(),
                depth: 0,
            }));
            continue;
        };
        // Extend the run downward over consecutive leaves sharing this group_uid.
        let run_top = i;
        let mut run_bottom = i;
        while run_bottom > 0 && flat[run_bottom - 1].group_uid.as_deref() == Some(uid.as_str()) {
            run_bottom -= 1;
        }
        if seen_groups.contains(&uid) {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] group {uid} is non-contiguous on the Z axis; rendering a split header"
            ));
        } else {
            seen_groups.push(uid.clone());
        }
        let (name, visible, collapsed) = stack
            .group_by_uid(&uid)
            .map(|g| (g.name.clone(), g.visible, g.collapsed))
            .unwrap_or_else(|| (uid.clone(), true, false));
        out.push(TreeItem::Group(GroupHeader {
            uid: uid.clone(),
            name,
            visible,
            collapsed,
            depth: 0,
        }));
        if !collapsed {
            for j in (run_bottom..=run_top).rev() {
                out.push(TreeItem::Leaf(Leaf {
                    kind: flat[j].kind.clone(),
                    depth: 1,
                }));
            }
        }
        i = run_bottom; // continue below the run
    }

    // Base layers close the list (they are the bottom of the composite), but REVERSED: the stack
    // holds them bottom-to-top (`Исходник`, then `Клин`) while every row above is top-to-bottom, so
    // emitting them raw would show `Клин` UNDER `Исходник` — the opposite of what is composited.
    out.extend(base.into_iter().rev().map(TreeItem::Leaf));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::{LayerKind, LayerStack};
    use eframe::egui::{Color32, ColorImage};

    /// A stack with only the two base layers, both 2x2 and transparent.
    fn base_stack() -> LayerStack {
        let size = [2, 2];
        let img = ColorImage::filled(size, Color32::TRANSPARENT);
        LayerStack::new(0, size, img.clone(), img)
    }

    #[test]
    fn clean_is_listed_above_source() {
        // Regression: the panel is top-to-bottom, so the base tail must be REVERSED relative to the
        // stack vector — `draw_composite` paints Clean OVER Source, and the panel must agree.
        let stack = base_stack();
        let tree = build_unified_tree(&stack, &[], &[]);
        let base_kinds: Vec<LayerKind> = tree
            .iter()
            .filter_map(|item| match item {
                TreeItem::Leaf(Leaf {
                    kind: LeafKind::Base(id),
                    ..
                }) => stack.layer(*id).map(|l| l.kind),
                TreeItem::Leaf(_) | TreeItem::Group(_) => None,
            })
            .collect();
        assert_eq!(
            base_kinds,
            vec![LayerKind::Clean, LayerKind::Source],
            "Клин must be listed above Исходник"
        );
    }

    #[test]
    fn base_layers_stay_at_the_bottom_below_every_raster() {
        let mut stack = base_stack();
        let a = stack.add_raster_layer();
        let b = stack.add_raster_layer();
        let bands = vec![
            Band::Raster {
                uid: stack.layer(a).expect("resident").uid.to_string(),
                z: 0,
            },
            Band::Raster {
                uid: stack.layer(b).expect("resident").uid.to_string(),
                z: 1,
            },
        ];
        let tree = build_unified_tree(&stack, &[], &bands);
        let kinds: Vec<&'static str> = tree
            .iter()
            .map(|item| match item {
                TreeItem::Group(_) => "group",
                TreeItem::Leaf(Leaf {
                    kind: LeafKind::Base(_),
                    ..
                }) => "base",
                TreeItem::Leaf(Leaf {
                    kind: LeafKind::Raster(_),
                    ..
                }) => "raster",
                TreeItem::Leaf(Leaf {
                    kind: LeafKind::Text(_),
                    ..
                }) => "text",
            })
            .collect();
        assert_eq!(kinds, vec!["raster", "raster", "base", "base"]);
    }
}
