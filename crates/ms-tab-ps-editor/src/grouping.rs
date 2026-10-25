/*
File: crates/ms-tab-ps-editor/src/grouping.rs

Purpose:
Band order and grouping edits of the layer stack (▲▼ moves, group create/ungroup/move) and their
persistence as structural edits.

Key functions:
- `band_order_after_move()`, `segment_blocks()`, `group_mut_by_uid()`: pure order helpers.
- `move_band_one()`, `move_group_block()`, `apply_group_op()`: in-memory mirror of a structural
  edit.
- `persist_grouping()`, `apply_structural_edit()`: the doc saver job for band/grouping edits (also
  rewrites text pin flags).

Notes:
Every text layer is its own band; order changes only through these moves.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// The band order after moving `target` one step (`up` = towards the top) in `order` (bottom-to-top,
/// each band with its group), plus whether it was a grouped intra-run swap.
///
/// A grouped band swaps with its adjacent row only inside its own group's run (`true`: the doc mirror
/// is one adjacent-node swap); an ungrouped band hops over the whole neighbouring block — a group
/// run or a single band (`false`: the doc mirror applies the whole order). `None` when `target` is
/// not in `order` or there is nothing to move past (already at the edge of its run / the stack).
pub(crate) fn band_order_after_move(
    mut order: Vec<(persist::BandRef, Option<String>)>,
    target: &persist::BandRef,
    up: bool,
) -> Option<(Vec<persist::BandRef>, bool)> {
    let pos = order.iter().position(|(b, _)| b == target)?;
    let my_group = order.get(pos)?.1.clone();
    let grouped_swap = my_group.is_some();
    if grouped_swap {
        // Reorder within the group's run only.
        let nb = if up { pos.checked_add(1)? } else { pos.checked_sub(1)? };
        if order.get(nb).is_some_and(|(_, g)| *g == my_group) {
            order.swap(pos, nb);
        } else {
            return None;
        }
    } else {
        // Ungrouped: hop over the neighbouring block.
        let blocks = segment_blocks(&order);
        let bi = blocks.iter().position(|(lo, hi)| pos >= *lo && pos <= *hi)?;
        let target_block = if up { bi.checked_add(1)? } else { bi.checked_sub(1)? };
        let &(nlo, nhi) = blocks.get(target_block)?;
        // Move our singleton block to the far side of the neighbour block. Moving up, removing our
        // (lower) row shifts the neighbour to [nlo-1, nhi-1], so inserting at `nhi` lands after it.
        let item = order.remove(pos);
        let insert_at = if up { nhi } else { nlo };
        order.insert(insert_at.min(order.len()), item);
    }
    Some((order.into_iter().map(|(b, _)| b).collect(), grouped_swap))
}

/// Segments a contiguous unified order into inclusive blocks `[lo, hi]`. A run of bands sharing the
/// same `Some(group)` is one block; every ungrouped band (`None`) is its own block.
pub(crate) fn segment_blocks(order: &[(persist::BandRef, Option<String>)]) -> Vec<(usize, usize)> {
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, (_, g)) in order.iter().enumerate() {
        if let Some((_, hi)) = blocks.last_mut()
            && g.is_some()
            && &order[*hi].1 == g
        {
            *hi = i;
        } else {
            blocks.push((i, i));
        }
    }
    blocks
}

/// Mutable group lookup by uid (`LayerStack` exposes only an immutable `group_by_uid`).
pub(crate) fn group_mut_by_uid<'a>(stack: &'a mut LayerStack, uid: &str) -> Option<&'a mut LayerGroup> {
    let gid = stack.group_by_uid(uid).map(|g| g.id)?;
    stack.group_mut(gid)
}

impl PsEditorTabState {
    /// Flattens a unified band `order` (bottom-to-top) into one node uid per band, expanding each
    /// `TextGroup(layer_idx)` band into its member text uids sub-ordered by ascending page-Y (lower on
    /// the page sorts lower in the stack), mirroring the render tiebreak (and the typing tab's
    /// `flatten_page_bands_to_refs`). Used to apply the SAME order the structure ops persist to disk onto the in-memory doc
    /// (whose nodes carry an explicit per-node Z, with no group-band concept).
    pub(crate) fn expand_order_to_node_uids(&self, order: &[persist::BandRef]) -> Vec<String> {
        let mut uids: Vec<String> = Vec::with_capacity(order.len());
        for band in order {
            match band {
                persist::BandRef::Raster(uid) | persist::BandRef::PinnedText(uid) => {
                    uids.push(uid.clone());
                }
                persist::BandRef::TextGroup(layer_idx) => {
                    let mut members: Vec<&PsTextLayer> = self
                        .text_layers
                        .iter()
                        .filter(|t| t.layer_idx == *layer_idx && !t.pinned)
                        .collect();
                    members.sort_by(|a, b| {
                        a.center()
                            .y
                            .partial_cmp(&b.center().y)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    uids.extend(members.iter().map(|t| t.uid.clone()));
                }
            }
        }
        uids
    }

    /// Current PS-group membership of every node (raster uid / text uid → group uid).
    pub(crate) fn current_membership(&self) -> HashMap<String, Option<String>> {
        let mut group_of: HashMap<String, Option<String>> = HashMap::new();
        if let Some(stack) = self.stack.as_ref() {
            for layer in stack.layers() {
                if layer.kind.is_base() {
                    continue;
                }
                group_of.insert(layer.uid.to_string(), stack.layer_group_uid(layer.id));
            }
        }
        for text in &self.text_layers {
            group_of.insert(text.uid.clone(), text.group_uid.clone());
        }
        group_of
    }

    /// The complete, contiguous structural band order (bottom-to-top) for the final membership
    /// `group_of`, each band paired with its final group, for callers that segment into group
    /// blocks. Thin wrapper over `tree::unified_band_order`: the SAME row order the panel shows
    /// (`tree::ordered_user_rows`, i.e. the canvas composite order), with a grouping edit's members
    /// pulled together at their group's lowest row; every text is its own `PinnedText` band.
    pub(crate) fn build_unified_order(
        &self,
        group_of: &HashMap<String, Option<String>>,
    ) -> Vec<(persist::BandRef, Option<String>)> {
        let Some(stack) = self.stack.as_ref() else {
            return Vec::new();
        };
        tree::unified_band_order(stack, &self.text_layers, &self.bands, group_of)
    }

    /// Moves a single raster or pinned-text band one step in Z. A grouped band reorders only within
    /// its group's run; an ungrouped band hops over the whole neighbouring block (group or band).
    ///
    // Z-reorder undo is a LATER part, not B1: this path enqueues a structural band-order saver job
    // (which must land AFTER the raster save so it wins) in addition to the doc mirror — a persistence
    // ordering the unified undo/redo persist tail cannot reproduce without a dedicated per-op
    // persistence hook. So no `PsEditOp` is recorded here yet.
    pub(crate) fn move_band_one(&mut self, sel: RowSel, up: bool, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        let target = match &sel {
            RowSel::Raster(id) => {
                let Some(uid) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(*id))
                    .map(|l| l.uid.to_string())
                else {
                    return;
                };
                persist::BandRef::Raster(uid)
            }
            RowSel::Text(uid) => persist::BandRef::PinnedText(uid.clone()),
            // A base layer has no band and is never reordered (it IS the bottom of the composite);
            // a group moves as a block through `move_group_block`, not here.
            RowSel::Base(_) | RowSel::Group(_) => return,
        };
        // Persist the stack's rasters (incl. the deletion `removed_uids`) BEFORE the band order: both
        // are saver jobs applied FIFO, and a pass applies rasters → text → effects → structural, so a
        // freshly-added raster already has its manifest node when the order lands (no sync flush).
        self.persist_current_page(project);
        let group_of = self.current_membership();
        let order = self.build_unified_order(&group_of);
        let Some((order_refs, grouped_swap)) = band_order_after_move(order, &target, up) else {
            return;
        };
        // The single node uid this band addresses (rasters / pinned text are single-node bands).
        let target_uid = match &target {
            persist::BandRef::Raster(u) | persist::BandRef::PinnedText(u) => Some(u.clone()),
            persist::BandRef::TextGroup(_) => None,
        };
        // Apply the SAME reorder in-memory so the doc (and, via its version bump, the typing tab)
        // re-project without a disk round-trip. A grouped intra-run move is one adjacent node swap; an
        // ungrouped block-hop reassigns the whole order.
        let node_order = self.expand_order_to_node_uids(&order_refs);
        let result = self.apply_structural_edit(
            page_idx,
            project,
            saver::StructuralEdit::BandOrder(order_refs),
            |doc| {
                if grouped_swap && let Some(uid) = &target_uid {
                    doc.reorder_node_one(page_idx, uid, up);
                } else {
                    doc.set_z_order(page_idx, &node_order);
                }
            },
        );
        if let Err(err) = result {
            ms_log::runtime_log::log_warn(format!("[ps_editor] move band: {err}"));
        }
    }

    /// Resolves a `GroupOp` into a `persist::GroupingEdit`, mirrors the raster-side changes into the
    /// in-memory stack, and persists it through `persist_grouping` (structural saver job + doc mirror).
    ///
    // Structural group ops (membership, create / delete, block moves) are OUT of scope for undo
    // Part B1 (they share the structural band-order job of `move_band_one`). The group META ops
    // (visibility, opacity) are doc-level and record `PsEditOp::GroupMeta`, like a raster's eye and
    // opacity slider.
    pub(crate) fn apply_group_op(&mut self, op: GroupOp, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        let group_of = self.current_membership();

        // Resolve the current selection into node uids (rasters + texts) and the raster ids.
        let mut sel_raster_ids: Vec<LayerId> = Vec::new();
        let mut sel_node_uids: Vec<String> = Vec::new();
        if let Some(stack) = self.stack.as_ref() {
            for s in &self.panel_selection {
                match s {
                    RowSel::Raster(id) => {
                        if let Some(l) = stack.layer(*id)
                            && !l.kind.is_base()
                        {
                            sel_raster_ids.push(*id);
                            sel_node_uids.push(l.uid.to_string());
                        }
                    }
                    RowSel::Text(uid) => sel_node_uids.push(uid.clone()),
                    // A base layer can never be grouped, ungrouped or deleted with a group. It
                    // cannot reach `panel_selection` in the first place (`select_row` /
                    // `selectable_row_order`); this arm keeps the lock explicit at the read site.
                    RowSel::Base(_) | RowSel::Group(_) => {}
                }
            }
        }
        // Text metadata: uid -> (currently pinned, pinned_by_group).
        let text_pin: HashMap<String, (bool, bool)> = self
            .text_layers
            .iter()
            .map(|t| (t.uid.clone(), (t.pinned, t.pinned_by_group)))
            .collect();
        let is_user_pinned = |uid: &str| text_pin.get(uid).is_some_and(|(p, pg)| *p && !*pg);
        let sel_text_uids: Vec<String> = sel_node_uids
            .iter()
            .filter(|u| text_pin.contains_key(*u))
            .cloned()
            .collect();

        let mut edit = persist::GroupingEdit::default();
        let mut new_gid: Option<(String, String)> = None; // (uid, name)

        // Group META ops. Visibility and opacity are the composite group fold every tab reads, so
        // they are DOC-level (`write_group_meta` -> `LayerDoc::set_group_meta`): the typing canvas,
        // the export and the next `sync_view_from_doc` all see them. The eye persists at once
        // (`route_to_doc`); an opacity drag mutates the doc per value change and defers the disk
        // write to the tab-switch flush, recording one undo step per gesture. Collapse is panel-only
        // VIEW state (no undo step) but has ONE owner, the doc (`set_group_collapsed`): it is written
        // there and persisted at once by the doc page save, so a later `route_to_doc` save can never
        // write a stale flag, and the stack copy is re-projected from the doc. Only without a resident
        // doc page does it fall back to the stack + `layers_dirty` (the stack-group persist).
        match &op {
            GroupOp::ToggleCollapse(uid) => {
                let collapsed = !self
                    .stack
                    .as_ref()
                    .and_then(|s| s.group_by_uid(uid))
                    .is_some_and(|g| g.collapsed);
                let routed = self.route_to_doc(page_idx, project, |doc| {
                    if !doc.set_group_collapsed(page_idx, uid, collapsed) {
                        ms_log::trace_log!(
                            cat::SYNC,
                            "toggle collapse: no doc group page={} uid={}",
                            page_idx,
                            uid
                        );
                    }
                });
                if !routed {
                    if let Some(g) = self.stack.as_mut().and_then(|s| group_mut_by_uid(s, uid)) {
                        g.collapsed = collapsed;
                    }
                    self.layers_dirty = true;
                }
                return;
            }
            GroupOp::ToggleGroupVisible(uid) => {
                if let Some(before) = self.group_meta_value(uid) {
                    let after = GroupMetaValue {
                        visible: !before.visible,
                        ..before
                    };
                    self.write_group_meta(page_idx, uid, after, Some(project));
                    self.history.record(PsEditOp::GroupMeta {
                        page_idx,
                        group_uid: uid.clone(),
                        before,
                        after,
                    });
                }
                return;
            }
            GroupOp::GroupOpacity(uid, v) => {
                // Snapshot the pre-drag meta ONCE per gesture (a drag on another group closes the
                // previous gesture first), so the whole drag is one undo step.
                if self
                    .group_opacity_gesture
                    .as_ref()
                    .is_none_or(|(gesture_page, gesture_uid, _)| *gesture_page != page_idx || gesture_uid != uid)
                {
                    self.finish_group_opacity_gesture();
                    if let Some(before) = self.group_meta_value(uid) {
                        self.group_opacity_gesture = Some((page_idx, uid.clone(), before));
                    }
                }
                if let Some(current) = self.group_meta_value(uid) {
                    let after = GroupMetaValue {
                        opacity: *v,
                        ..current
                    };
                    self.write_group_meta(page_idx, uid, after, None);
                }
                return;
            }
            GroupOp::MoveGroup(uid, up) => {
                self.move_group_block(uid.clone(), *up, project);
                return;
            }
            _ => {}
        }

        // Membership-changing ops below all rebuild the unified order.
        let mut final_group = group_of.clone();

        let target_group: Option<String> = match &op {
            GroupOp::NewFromSelection => {
                let n = self.stack.as_ref().map_or(0, |s| s.groups().len()) + 1;
                let uid = uuid::Uuid::new_v4().to_string();
                // Persisted group name (round-trips to `layers.json`); stable literal.
                // See dev-docs/i18n_exclusions.md §A (persisted layer/group names).
                let name = format!("Группа {n}");
                edit.new_groups.push(persist::GroupMeta {
                    uid: uid.clone(),
                    name: name.clone(),
                    visible: true,
                    opacity: 1.0,
                    collapsed: false,
                });
                new_gid = Some((uid.clone(), name));
                Some(uid)
            }
            GroupOp::MoveTo(uid) => Some(uid.clone()),
            GroupOp::Ungroup => None,
            GroupOp::DeleteGroup(uid) => {
                edit.remove_groups.push(uid.clone());
                // Members of the deleted group ungroup and lose their `pinned_by_group` mark. They
                // stay pinned at their visible Z: the order below names every text as its own
                // `PinnedText` band (`tree::unified_band_order`), so nothing jumps on the canvas.
                let members: Vec<String> = final_group
                    .iter()
                    .filter(|(_, g)| g.as_deref() == Some(uid.as_str()))
                    .map(|(n, _)| n.clone())
                    .collect();
                for n in &members {
                    edit.set_membership.push((n.clone(), None));
                    final_group.insert(n.clone(), None);
                    if text_pin.get(n).is_some_and(|(_, pg)| *pg) {
                        edit.unpin_for_group.push(n.clone());
                    }
                }
                // Mirror: removing the group ungroups its raster members in the stack too.
                if let Some(stack) = self.stack.as_mut()
                    && let Some(gid) = stack.group_by_uid(uid).map(|g| g.id)
                {
                    stack.remove_group(gid);
                }
                let order = self
                    .build_unified_order(&final_group)
                    .into_iter()
                    .map(|(b, _)| b)
                    .collect();
                edit.order = order;
                self.persist_grouping(edit, page_idx, project);
                return;
            }
            _ => None,
        };

        // Apply membership for NewFromSelection / MoveTo / Ungroup.
        for uid in &sel_node_uids {
            edit.set_membership
                .push((uid.clone(), target_group.clone()));
            final_group.insert(uid.clone(), target_group.clone());
        }
        if target_group.is_some() {
            // Entering a group: mark the texts the group pinned (every text is its own band in the
            // order anyway), so a later ungroup can tell a group pin from a user pin.
            for uid in &sel_text_uids {
                if !is_user_pinned(uid) {
                    edit.pin_for_group.push(uid.clone());
                }
            }
        } else {
            // Ungroup: clear the group-pin mark (the text keeps its own band at its visible Z).
            for uid in &sel_text_uids {
                if text_pin.get(uid).is_some_and(|(_, pg)| *pg) {
                    edit.unpin_for_group.push(uid.clone());
                }
            }
        }

        let order = self
            .build_unified_order(&final_group)
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        edit.order = order;

        // Mirror raster membership / group creation into the stack.
        if let Some(stack) = self.stack.as_mut() {
            let target_gid = match (&op, &new_gid, &target_group) {
                (GroupOp::NewFromSelection, Some((uid, name)), _) => {
                    let parsed =
                        uuid::Uuid::parse_str(uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                    Some(stack.add_group_with_uid(name.clone(), parsed))
                }
                (_, _, Some(uid)) => stack.group_by_uid(uid).map(|g| g.id),
                _ => None,
            };
            for id in &sel_raster_ids {
                stack.set_layer_group(*id, target_gid);
            }
        }

        self.persist_grouping(edit, page_idx, project);
    }

    /// Moves a group's whole contiguous block one step in Z by swapping it with the neighbouring
    /// block, then persists the resulting band order.
    pub(crate) fn move_group_block(&mut self, uid: String, up: bool, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Persist the stack's rasters (incl. the deletion `removed_uids`) BEFORE the band order: both
        // are saver jobs applied FIFO, and a pass applies rasters → text → effects → structural, so a
        // freshly-added raster already has its manifest node when the order lands (no sync flush).
        self.persist_current_page(project);
        let group_of = self.current_membership();
        let bands = self.build_unified_order(&group_of);
        let blocks = segment_blocks(&bands);
        let Some(bi) = blocks
            .iter()
            .position(|(lo, _)| bands[*lo].1.as_deref() == Some(uid.as_str()))
        else {
            return;
        };
        let target = if up { bi + 1 } else { bi.wrapping_sub(1) };
        if target >= blocks.len() {
            return;
        }
        // Rebuild the order with the two blocks swapped.
        let mut order_blocks: Vec<Vec<(persist::BandRef, Option<String>)>> = blocks
            .iter()
            .map(|(lo, hi)| bands[*lo..=*hi].to_vec())
            .collect();
        order_blocks.swap(bi, target);
        let order_refs: Vec<persist::BandRef> =
            order_blocks.into_iter().flatten().map(|(b, _)| b).collect();
        // Apply the same group-block move in-memory so the doc (and, via its version bump, the typing
        // tab) re-project without a disk round-trip.
        let result = self.apply_structural_edit(
            page_idx,
            project,
            saver::StructuralEdit::BandOrder(order_refs),
            |doc| {
                // `false` (the doc found no block to move) is tolerated exactly as before: the view
                // re-projects from the doc either way.
                doc.reorder_group_block(page_idx, &uid, up);
            },
        );
        if let Err(err) = result {
            ms_log::runtime_log::log_warn(format!("[ps_editor] move group: {err}"));
        }
    }

    /// Persists a grouping edit and mirrors the SAME edit onto the shared doc in-memory (so it and the
    /// typing tab re-project without a disk round-trip). The stack's rasters are persisted first so
    /// freshly-added raster layers already have manifest nodes for the edit's membership / order to
    /// land on (FIFO saver jobs; see `apply_structural_edit`).
    pub(crate) fn persist_grouping(
        &mut self,
        edit: persist::GroupingEdit,
        page_idx: usize,
        project: &ProjectData,
    ) {
        self.persist_current_page(project);
        // Snapshot the in-memory doc effect of `edit` BEFORE it is moved into the structural job. The
        // band order expansion uses the current text-layer page-Y order (unchanged by membership),
        // matching what `save_page_grouping`'s `apply_band_order` records on disk.
        let node_order = self.expand_order_to_node_uids(&edit.order);
        let new_groups = edit.new_groups.clone();
        let remove_groups = edit.remove_groups.clone();
        let set_membership = edit.set_membership.clone();
        // Apply removes → creates → membership → order, mirroring `save_page_grouping`.
        let result = self.apply_structural_edit(
            page_idx,
            project,
            saver::StructuralEdit::Grouping(edit),
            |doc| {
                for g in &remove_groups {
                    doc.remove_group(page_idx, g);
                }
                for g in new_groups {
                    doc.add_group(page_idx, g);
                }
                for (node_uid, group_uid) in &set_membership {
                    doc.set_group(page_idx, node_uid, group_uid.clone());
                }
                if !node_order.is_empty() {
                    doc.set_z_order(page_idx, &node_order);
                }
            },
        );
        if let Err(err) = result {
            ms_log::runtime_log::log_warn(format!("[ps_editor] grouping: {err}"));
        }
    }

    /// Text pin meta `(uid, pinned, pinned_by_group)` of every local text runtime AFTER `edit` is
    /// applied, mirroring the manifest rules exactly: a band order re-derives every text's `pinned`
    /// from whether a `PinnedText` band names it (`persist::apply_band_order`; a grouping edit only
    /// when its `order` is non-empty), then a grouping edit sets `pinned_by_group` for
    /// `pin_for_group` and clears it for `unpin_for_group` (clear wins, as it is applied second).
    pub(crate) fn text_pin_meta_after(&self, edit: &saver::StructuralEdit) -> Vec<(String, bool, bool)> {
        let empty: &[String] = &[];
        let (order, pin_for_group, unpin_for_group): (Option<&[persist::BandRef]>, &[String], &[String]) =
            match edit {
                saver::StructuralEdit::BandOrder(order) => (Some(order.as_slice()), empty, empty),
                saver::StructuralEdit::Grouping(g) => (
                    (!g.order.is_empty()).then_some(g.order.as_slice()),
                    g.pin_for_group.as_slice(),
                    g.unpin_for_group.as_slice(),
                ),
            };
        self.text_layers
            .iter()
            .map(|t| {
                let pinned = order.map_or(t.pinned, |order| {
                    order
                        .iter()
                        .any(|b| matches!(b, persist::BandRef::PinnedText(u) if *u == t.uid))
                });
                let mut pinned_by_group = t.pinned_by_group;
                if pin_for_group.contains(&t.uid) {
                    pinned_by_group = true;
                }
                if unpin_for_group.contains(&t.uid) {
                    pinned_by_group = false;
                }
                (t.uid.clone(), pinned, pinned_by_group)
            })
            .collect()
    }

    /// The single funnel for PS STRUCTURAL page edits (band order / grouping; band moves, group-block
    /// moves and grouping ops). Inside ONE `edit_doc_node` doc edit it enqueues the
    /// structural saver job (`enqueue_page_band_order` / `enqueue_page_grouping` — applied after the
    /// page's earlier raster / text / effects parts; a saver-less doc writes synchronously there) and,
    /// only when that succeeded, runs `mirror` (the in-memory model change) and pushes the resulting
    /// text pin meta into the doc (`set_text_pin_meta`), so the doc — the owner of that meta — stays
    /// equal to what the job writes. The local text runtimes get the same pin meta afterwards. There
    /// is NO disk read and, with a saver, no synchronous write.
    ///
    /// Without a doc (or with the page not resident) it falls back to the synchronous
    /// `persist::save_page_band_order` / `save_page_grouping` and updates only the local runtimes.
    ///
    /// # Errors
    /// Only a synchronous write (saver-less doc or no doc) can fail; the message is returned for the
    /// caller to log, and neither the doc nor the local pin meta changes.
    pub(crate) fn apply_structural_edit<F>(
        &mut self,
        page_idx: usize,
        project: &ProjectData,
        edit: saver::StructuralEdit,
        mirror: F,
    ) -> Result<(), String>
    where
        F: FnOnce(&mut ms_models::layer_model::layer_doc::LayerDoc),
    {
        let pin_meta = self.text_pin_meta_after(&edit);
        let layers_dir = project.paths.unsaved_layers_dir.clone();
        let committed_dir = project.paths.layers_dir.clone();
        // `Some` until the doc closure consumes it: a closure that never ran (no doc / page not
        // resident) leaves it here for the synchronous fallback.
        let mut pending = Some(edit);
        let mut result: Result<(), String> = Ok(());
        self.edit_doc_node(page_idx, |doc| {
            let Some(edit) = pending.take() else {
                return;
            };
            result = match edit {
                saver::StructuralEdit::BandOrder(order) => {
                    doc.enqueue_page_band_order(page_idx, &layers_dir, Some(&committed_dir), order)
                }
                saver::StructuralEdit::Grouping(grouping) => {
                    doc.enqueue_page_grouping(page_idx, &layers_dir, Some(&committed_dir), grouping)
                }
            };
            if result.is_ok() {
                mirror(doc);
                for (uid, pinned, pinned_by_group) in &pin_meta {
                    if !doc.set_text_pin_meta(page_idx, uid, *pinned, *pinned_by_group) {
                        // A local runtime without a doc text node: the runtimes are materialized
                        // from this very page, so this only means the node left the doc concurrently.
                        ms_log::trace_log!(
                            cat::SYNC,
                            "structural edit: no doc text node for pin meta page={} uid={}",
                            page_idx,
                            uid
                        );
                    }
                }
            }
        });
        if let Some(edit) = pending.take() {
            // Doc-less or non-resident page (PS decode failure): the edit is written synchronously and
            // bypasses the saver FIFO — logged so a stale-ordering report can be traced to this path.
            ms_log::trace_log!(
                cat::SYNC,
                "structural edit: sync fallback write (no resident doc page) page={}",
                page_idx
            );
            result = match edit {
                saver::StructuralEdit::BandOrder(order) => {
                    persist::save_page_band_order(&layers_dir, page_idx, &order)
                }
                saver::StructuralEdit::Grouping(grouping) => {
                    persist::save_page_grouping(&layers_dir, page_idx, &grouping)
                }
            };
        }
        result?;
        let by_uid: HashMap<String, (bool, bool)> = pin_meta
            .into_iter()
            .map(|(uid, pinned, pinned_by_group)| (uid, (pinned, pinned_by_group)))
            .collect();
        for text in &mut self.text_layers {
            if let Some(&(pinned, pinned_by_group)) = by_uid.get(&text.uid) {
                text.pinned = pinned;
                text.pinned_by_group = pinned_by_group;
            }
        }
        Ok(())
    }
}
