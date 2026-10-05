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
group runs bracketed by headers. The bottom-to-top leaf order comes from the SAME owner the canvas
draws with (`ms_models::layer_model::ordering::composite_plan`): band Z (uid-first lookup), then
rasters below texts, then input order (rasters in stack order, texts in `text_layers` order), so
the panel order equals the composite order. Hidden / dimmed rows are still listed: the tree asks the
owner for ORDER only (every item passed visible, opacity 1, ungrouped).

That row order has ONE builder here, `ordered_user_rows`; both the panel (`build_unified_tree`) and
the structural band order the ▲▼ / grouping ops persist (`unified_band_order`, wrapped by the tab's
`build_unified_order`) derive from it, so a move always acts on the neighbour the user sees.

The WHOLE emitted list is top-to-bottom, the base tail included: the base layers are appended last
(they are the bottom of the composite) but in REVERSE stack order, so `Клин` — which `draw_composite`
paints OVER `Исходник` — is listed above it. The `LayerStack` vector order itself is never changed:
it is load-bearing for compositing, and only this view is reordered.
*/

use super::layers::{LayerId, LayerStack};
use super::text_layers::PsTextLayer;
use ms_models::layer_model::ordering::{self, Band, CompositeItem, CompositeKey};
use ms_models::layer_model::persist::BandRef;
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

/// One USER row (raster or text, never a base layer) in bottom-to-top composite order: the single
/// order both the panel tree (`build_unified_tree`) and the structural band order
/// (`unified_band_order`) are derived from.
#[derive(Debug, Clone)]
pub struct OrderedRow {
    /// `LeafKind::Raster` or `LeafKind::Text`; never `LeafKind::Base`.
    pub kind: LeafKind,
    /// The node uid (raster layer uid / text overlay uid).
    pub uid: String,
    /// The row's CURRENT unified PS group uid.
    pub group_uid: Option<String>,
}

/// An order-only composite item: visible, fully opaque and ungrouped, so `composite_plan` keeps it
/// and only its band Z / kind / input position decide where it lands.
fn order_only(key: CompositeKey<'_>) -> CompositeItem<'_> {
    CompositeItem {
        key,
        group_uid: None,
        visible: true,
        opacity: 1.0,
    }
}

/// The user rows of the page bottom-to-top in EXACTLY the canvas composite order: rows enter the
/// shared owner (`ordering::composite_plan`) in `draw_composite`'s input order — user rasters in
/// stack order, then texts in `text_layers` order — as order-only items, so band Z (uid-first),
/// Raster < Text at equal Z and input order on ties decide, and hidden / dimmed rows are kept.
#[must_use]
pub fn ordered_user_rows(
    stack: &LayerStack,
    text_layers: &[PsTextLayer],
    bands: &[Band],
) -> Vec<OrderedRow> {
    let mut candidates: Vec<OrderedRow> = Vec::with_capacity(stack.layers().len() + text_layers.len());
    for layer in stack.layers().iter().filter(|layer| !layer.kind.is_base()) {
        candidates.push(OrderedRow {
            kind: LeafKind::Raster(layer.id),
            uid: layer.uid.to_string(),
            group_uid: stack.layer_group_uid(layer.id),
        });
    }
    for (index, text) in text_layers.iter().enumerate() {
        candidates.push(OrderedRow {
            kind: LeafKind::Text(index),
            uid: text.uid.clone(),
            group_uid: text.group_uid.clone(),
        });
    }
    let order: Vec<usize> = {
        let items: Vec<CompositeItem<'_>> = candidates
            .iter()
            .map(|row| match row.kind {
                // `index` is the row's own position in `text_layers` (built just above).
                LeafKind::Text(index) => order_only(CompositeKey::Text {
                    uid: &row.uid,
                    layer_idx: text_layers.get(index).map_or(0, |t| t.layer_idx),
                }),
                LeafKind::Raster(_) | LeafKind::Base(_) => order_only(CompositeKey::Raster { uid: &row.uid }),
            })
            .collect();
        // Every item is visible, opaque and ungrouped, so the plan omits none of them.
        ordering::composite_plan(bands, &[], &items)
            .iter()
            .map(|step| step.item)
            .collect()
    };
    let mut slots: Vec<Option<OrderedRow>> = candidates.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|item| slots.get_mut(item).and_then(Option::take))
        .collect()
}

/// The structural band order (bottom-to-top) for the given FINAL group membership `group_of`
/// (node uid -> group uid; a uid absent from the map is ungrouped), paired with each band's group.
///
/// Derived from [`ordered_user_rows`], so with unchanged membership it IS the panel's row order;
/// each group's members are then pulled together at the group's lowest member row (stable), which
/// only moves rows when a grouping edit changed membership. Every text row is a
/// `BandRef::PinnedText`: a text is its own row at its own Z (the order the user sees), and
/// persisting the order pins it there (`persist::apply_band_order`).
#[must_use]
pub fn unified_band_order(
    stack: &LayerStack,
    text_layers: &[PsTextLayer],
    bands: &[Band],
    group_of: &HashMap<String, Option<String>>,
) -> Vec<(BandRef, Option<String>)> {
    let rows = ordered_user_rows(stack, text_layers, bands);
    let group_at: Vec<Option<String>> = rows
        .iter()
        .map(|row| group_of.get(&row.uid).cloned().flatten())
        .collect();
    // A group's anchor is the position of its lowest member row.
    let mut anchor: HashMap<&str, usize> = HashMap::new();
    for (position, group) in group_at.iter().enumerate() {
        if let Some(uid) = group {
            anchor.entry(uid.as_str()).or_insert(position);
        }
    }
    let mut keyed: Vec<(usize, usize)> = group_at
        .iter()
        .enumerate()
        .map(|(position, group)| {
            let key = group
                .as_deref()
                .and_then(|uid| anchor.get(uid).copied())
                .unwrap_or(position);
            (key, position)
        })
        .collect();
    keyed.sort_unstable();
    keyed
        .into_iter()
        .filter_map(|(_, position)| {
            let row = rows.get(position)?;
            let band = match row.kind {
                LeafKind::Raster(_) | LeafKind::Base(_) => BandRef::Raster(row.uid.clone()),
                LeafKind::Text(_) => BandRef::PinnedText(row.uid.clone()),
            };
            Some((band, group_at.get(position).cloned().flatten()))
        })
        .collect()
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
    // Base layers are the very bottom and are not bands: they close the list below.
    let base: Vec<Leaf> = stack
        .layers()
        .iter()
        .filter(|layer| layer.kind.is_base())
        .map(|layer| Leaf {
            kind: LeafKind::Base(layer.id),
            depth: 0,
        })
        .collect();

    let flat = ordered_user_rows(stack, text_layers, bands);

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
