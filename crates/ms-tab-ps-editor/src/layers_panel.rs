/*
File: crates/ms-tab-ps-editor/src/layers_panel.rs

Purpose:
The «Слои» panel: row model, selection rules, the active-layer control strip and the deferred panel
actions.

Key functions:
- `selectable_row_order()`, `row_is_selected()`, `group_row_is_selected()`: pure selection/highlight
  rules.
- `layers_panel_body()`, `build_panel_rows()`, `draw_group_row()`, `draw_leaf_row()`: the panel UI.
- `select_row()`, `normalize_panel_primary()`, `active_row_sel()`: the primary row / multi-selection
  contract.
- `draw_active_controls()`, merge candidates, `apply_panel_actions()`: actions resolved after the
  panel closure ends.

Notes:
Base rows are selectable but structurally locked (`RowSel::is_base`).
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// The order the panel's Shift-range multi-selection walks, top-to-bottom.
///
/// BASE ROWS ARE EXCLUDED. They are structurally locked, so they must never end up in
/// `panel_selection` — and a Shift range that swept the bottom of the list would put them there.
/// Together with `select_row`'s own base guard this is the replacement for the older lock, where a
/// base leaf simply had no `RowSel` at all.
pub(crate) fn selectable_row_order(rows: &[PanelRow]) -> Vec<RowSel> {
    rows.iter()
        .filter_map(|r| match r {
            PanelRow::Group(h) => Some(RowSel::Group(h.uid.clone())),
            PanelRow::Leaf(l) => l.sel.clone().filter(|s| !s.is_base()),
        })
        .collect()
}

/// Whether a leaf row is drawn as selected.
///
/// A row is highlighted when it is in the multi-selection OR when it is the primary row. The
/// primary term is what makes the ALWAYS-PRESENT active layer visible: a freshly loaded page seeds
/// `panel_primary` (to `Клин`) while deliberately leaving `panel_selection` empty, so a
/// selection-only rule would leave the active layer unmarked.
///
/// `sel` is `None` only for a row that has no selection key at all (a text leaf whose runtime
/// vanished between the tree build and the snapshot); such a row is never highlighted.
pub(crate) fn row_is_selected(
    sel: Option<&RowSel>,
    selection: &HashSet<RowSel>,
    primary: Option<&RowSel>,
) -> bool {
    sel.is_some_and(|s| selection.contains(s) || primary == Some(s))
}

/// Whether a group-header row is drawn as selected.
///
/// Beyond the leaf rule it carries one extra case: a COLLAPSED group emits no rows for its members
/// (`tree::build_unified_tree`), so a primary row inside it would be invisible. The header borrows
/// the highlight in that case. `primary_group_uid` is the group uid the primary row belongs to, or
/// `None` when the primary is ungrouped, is a group header itself, or is a base layer (base layers
/// are never grouped).
pub(crate) fn group_row_is_selected(
    uid: &str,
    selection: &HashSet<RowSel>,
    primary: Option<&RowSel>,
    collapsed: bool,
    primary_group_uid: Option<&str>,
) -> bool {
    let own = RowSel::Group(uid.to_owned());
    if selection.contains(&own) || primary == Some(&own) {
        return true;
    }
    collapsed && primary_group_uid == Some(uid)
}

impl PsEditorTabState {
    /// Sets the selection to the active layer's full footprint. When the panel's primary row is a TEXT
    /// layer, the footprint is that overlay's page-space outline (`PsTextLayer::footprint_polygon`),
    /// since text layers are NOT in `LayerStack`. Otherwise it falls back to the stack's active layer: a
    /// page rectangle for a base / page-sized identity layer, or the (possibly rotated/scaled) image
    /// quad for a transformed raster layer.
    pub(crate) fn select_active_layer_fully(&mut self) {
        let Some(page) = self.stack.as_ref().map(LayerStack::size) else {
            return;
        };
        // Text layers live outside the stack: use the primary-row text overlay's footprint directly.
        if let Some(RowSel::Text(uid)) = &self.panel_primary {
            // Collect the polygon before borrowing `self.selection` mutably (no overlapping borrows).
            let polygon = self
                .text_layers
                .iter()
                .find(|t| t.uid() == uid)
                .map(PsTextLayer::footprint_polygon);
            if let Some(polygon) = polygon {
                let mut selection = Selection::empty(page[0], page[1]);
                selection.set_polygon(&polygon);
                // An overlay dragged fully off the page has an empty footprint: store `None`, never
                // an invisible all-zero mask that would keep clipping the brush.
                self.set_selection(selection);
            }
            return;
        }
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        let active = stack.active_id();
        let Some(layer) = stack.layer(active) else {
            return;
        };
        let mut selection = Selection::empty(page[0], page[1]);
        if layer.image.size == page && layer.transform.is_identity_for(layer.image.size) {
            selection.set_rect(0, 0, page[0] as i32, page[1] as i32);
        } else {
            let pts: Vec<(f32, f32)> = layer.world_corners().iter().map(|p| (p.x, p.y)).collect();
            selection.set_polygon(&pts);
        }
        // Same invariant as above: a raster moved entirely off-page selects nothing at all.
        self.set_selection(selection);
    }

    /// «Слои» body: the unified, Photoshop-like layer tree. Compact rows (eye + name + group
    /// indent), collapsible/movable groups that may mix rasters and texts, Shift/Ctrl multi-select,
    /// a right-click menu for grouping, and a controls strip for the active layer.
    ///
    /// Returns the deferred actions; the caller applies them with `apply_panel_actions` after the
    /// dock frame. The tree + per-row data are snapshotted into owned values first, so the render
    /// loop can mutate `self.panel_selection` without holding any borrow of `self.stack` /
    /// `self.text_layers`.
    pub(crate) fn layers_panel_body(&mut self, ui: &mut egui::Ui) -> PanelActions {
        let mut actions = PanelActions::default();
        if self.stack.is_none() {
            ui.label(t!("ps_editor.layers_panel.no_page"));
            return actions;
        }

        ui.horizontal(|ui| {
            if ui
                .button(t!("ps_editor.layers_panel.add_layer_button"))
                .clicked()
            {
                actions.add_layer = true;
            }
            if ui
                .button(t!("ps_editor.layers_panel.add_group_button"))
                .clicked()
            {
                actions.new_empty_group = true;
            }
        });
        ui.separator();

        // Owned snapshot: rows (top-to-bottom), the selectable-row order (for Shift range), and the
        // existing-group list (for the "move to group" submenu).
        let rows = self.build_panel_rows();
        let row_sels = selectable_row_order(&rows);
        let group_list: Vec<(String, String)> = self
            .stack
            .as_ref()
            .map(|s| {
                s.groups()
                    .iter()
                    .map(|g| (g.uid.to_string(), g.name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let selection = self.panel_selection.clone();
        let primary = self.panel_primary.clone();
        // The group a COLLAPSED header must borrow the primary highlight from: a collapsed group
        // emits no member rows, so without this the primary row inside it would be invisible.
        // A base primary contributes nothing — base layers are never grouped.
        let primary_group_uid: Option<String> = match &primary {
            Some(RowSel::Raster(id)) => self.stack.as_ref().and_then(|s| s.layer_group_uid(*id)),
            Some(RowSel::Text(uid)) => self
                .text_layers
                .iter()
                .find(|t| t.uid() == uid)
                .and_then(|t| t.group_uid.clone()),
            Some(RowSel::Base(_) | RowSel::Group(_)) | None => None,
        };

        // NO nested scroll area and NO hand-computed height reserve here. The dock already draws
        // every tab body inside a bounded `ScrollArea::both` and derives the panel's size request
        // from the CONTENT's measured height, so a fixed-height inner scroll area would fight that
        // measurement and make the panel oscillate or never grow
        // (`panel_dock/MODULE_README.md`, «The body FILLS its budget…»). The intended height is
        // expressed by `PS_EDITOR_LAYERS_TAB_MIN_SIZE_PX` / `..._INITIAL_SIZE_PX` instead.
        let cx = PanelRowCx {
            selection: &selection,
            primary: primary.as_ref(),
            primary_group_uid: primary_group_uid.as_deref(),
            row_sels: &row_sels,
            group_list: &group_list,
        };
        for row in &rows {
            match row {
                PanelRow::Group(h) => {
                    self.draw_group_row(ui, h, &cx, &mut actions);
                }
                PanelRow::Leaf(leaf) => {
                    self.draw_leaf_row(ui, leaf, &cx, &mut actions);
                }
            }
        }

        ui.separator();
        self.draw_active_controls(ui, &mut actions);
        actions
    }

    /// Builds the owned per-row snapshot from the unified tree + stack + text layers.
    ///
    /// Every leaf that has an identity gets a `RowSel`, base layers included (`RowSel::Base`) — a
    /// base row must be able to be the primary row, because the editor always has an active layer
    /// and it defaults to `Клин`. `sel` is `None` only for a text leaf whose runtime is missing.
    pub(crate) fn build_panel_rows(&self) -> Vec<PanelRow> {
        let Some(stack) = self.stack.as_ref() else {
            return Vec::new();
        };
        let tree = tree::build_unified_tree(stack, &self.text_layers, &self.bands);
        tree.into_iter()
            .map(|item| match item {
                tree::TreeItem::Group(h) => PanelRow::Group(h),
                tree::TreeItem::Leaf(leaf) => {
                    let (sel, name, visible, is_base) = match &leaf.kind {
                        tree::LeafKind::Base(id) => {
                            let l = stack.layer(*id);
                            (
                                // A base row IS keyed: it must be able to become the primary row so
                                // the always-present active layer is visible. Its structural lock is
                                // re-expressed at every consumer via `RowSel::is_base`.
                                Some(RowSel::Base(*id)),
                                l.map_or_else(
                                    || t!("ps_editor.layers_panel.leaf_fallback_layer").into(),
                                    |l| l.name.clone(),
                                ),
                                l.is_some_and(|l| l.visible),
                                true,
                            )
                        }
                        tree::LeafKind::Raster(id) => {
                            let l = stack.layer(*id);
                            (
                                Some(RowSel::Raster(*id)),
                                l.map_or_else(
                                    || t!("ps_editor.layers_panel.leaf_fallback_raster").into(),
                                    |l| l.name.clone(),
                                ),
                                l.is_some_and(|l| l.visible),
                                false,
                            )
                        }
                        tree::LeafKind::Text(i) => {
                            let t = self.text_layers.get(*i);
                            // Show a text preview (`Текст (preview)`) using the same logic as the typing
                            // tab; fall back to the stored node name when the overlay has no text. The
                            // `🅣` icon is added later in `draw_leaf_row`, so it is omitted here.
                            let name = t.map_or_else(
                                || t!("ps_editor.layers_panel.leaf_fallback_text").into(),
                                |t| {
                                    let preview = ms_tab_typing::text_preview_label(
                                        &t.text_content,
                                        PS_TEXT_PREVIEW_CHARS,
                                    );
                                    if preview.is_empty() {
                                        t.name.clone()
                                    } else {
                                        tf!(
                                            "ps_editor.layers_panel.text_preview",
                                            preview = preview
                                        )
                                    }
                                },
                            );
                            (
                                t.map(|t| RowSel::Text(t.uid.clone())),
                                name,
                                t.is_some_and(|t| t.visible),
                                false,
                            )
                        }
                    };
                    PanelRow::Leaf(PanelLeaf {
                        sel,
                        kind: leaf.kind,
                        depth: leaf.depth,
                        name,
                        visible,
                        is_base,
                    })
                }
            })
            .collect()
    }

    /// One group-header row: collapse arrow, visibility eye, name, block move arrows, context menu.
    ///
    /// A COLLAPSED header also carries the highlight of a primary row hidden inside it
    /// (`group_row_is_selected`), because a collapsed group emits no member rows.
    pub(crate) fn draw_group_row(
        &mut self,
        ui: &mut egui::Ui,
        header: &tree::GroupHeader,
        cx: &PanelRowCx<'_>,
        actions: &mut PanelActions,
    ) {
        let selected = group_row_is_selected(
            &header.uid,
            cx.selection,
            cx.primary,
            header.collapsed,
            cx.primary_group_uid,
        );
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(header.depth as f32 * tree::INDENT);
                let arrow = if header.collapsed { "▸" } else { "▾" };
                if ui
                    .add(egui::Button::new(arrow).small().frame(false))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::ToggleCollapse(header.uid.clone()));
                }
                let mut vis = header.visible;
                if ui.checkbox(&mut vis, "").changed() {
                    actions.group_op = Some(GroupOp::ToggleGroupVisible(header.uid.clone()));
                }
                let label = ui.selectable_label(selected, format!("📁 {}", header.name));
                if ui.add(egui::Button::new("▲").small()).clicked() {
                    actions.group_op = Some(GroupOp::MoveGroup(header.uid.clone(), true));
                }
                if ui.add(egui::Button::new("▼").small()).clicked() {
                    actions.group_op = Some(GroupOp::MoveGroup(header.uid.clone(), false));
                }
                label
            })
            .inner;
        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            self.select_row(RowSel::Group(header.uid.clone()), mods, &[]);
        }
        egui::Popup::context_menu(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if ui
                    .button(t!("ps_editor.layers_panel.delete_group_button"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::DeleteGroup(header.uid.clone()));
                    egui::Popup::close_all(ui.ctx());
                }
            });
    }

    /// One leaf row (raster / text / locked base): visibility eye + indented name + selection +
    /// right-click grouping menu.
    ///
    /// A base row is clickable and CAN become the primary row (that is how the always-present active
    /// layer is shown), but it returns before the grouping context menu: it is structurally locked.
    pub(crate) fn draw_leaf_row(
        &mut self,
        ui: &mut egui::Ui,
        leaf: &PanelLeaf,
        cx: &PanelRowCx<'_>,
        actions: &mut PanelActions,
    ) {
        let selected = row_is_selected(leaf.sel.as_ref(), cx.selection, cx.primary);
        // Exhaustive on purpose (no `_ =>`): a new `LeafKind` must be given an icon deliberately.
        let icon = match leaf.kind {
            tree::LeafKind::Text(_) => "🅣",
            tree::LeafKind::Raster(_) | tree::LeafKind::Base(_) => "▦",
        };
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(leaf.depth as f32 * tree::INDENT + tree::INDENT);
                // Visibility toggle (base layers can be hidden but not edited).
                let mut vis = leaf.visible;
                if ui.checkbox(&mut vis, "").changed() {
                    match &leaf.kind {
                        tree::LeafKind::Raster(id) | tree::LeafKind::Base(id) => {
                            actions.toggle_visible_raster = Some(*id);
                        }
                        tree::LeafKind::Text(i) => actions.toggle_visible_text = Some(*i),
                    }
                }
                let label = ui.selectable_label(selected, format!("{icon} {}", leaf.name));
                if leaf.is_base {
                    ui.label("🔒");
                }
                label
            })
            .inner;

        let Some(sel) = leaf.sel.clone() else {
            // No selection key at all: a text leaf whose runtime vanished between the tree build and
            // this snapshot. Nothing to select and nothing to act on.
            return;
        };

        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            // `select_row` keeps a base row a SOLO primary (never in `panel_selection`), so the
            // modifiers are safe to forward for every kind.
            self.select_row(sel.clone(), mods, cx.row_sels);
            match leaf.kind {
                // A base row makes its layer active exactly like a raster row does — `Клин` is
                // paintable, and `Исходник` is the read-only layer the user may still want selected.
                tree::LeafKind::Raster(id) | tree::LeafKind::Base(id) => {
                    actions.set_active_raster = Some(id);
                }
                tree::LeafKind::Text(_) => {}
            }
            // Cover raster, base AND text rows: show the clicked (primary) layer's marquee immediately.
            actions.request_select_active = true;
        }
        if sel.is_base() {
            // One site of the structural lock: no right-click selection, no grouping menu — a base
            // layer can never be grouped, ungrouped or moved into a group.
            return;
        }
        // Right-click acts on the current multi-selection; if this row is not selected, select it.
        if resp.secondary_clicked() && !self.panel_selection.contains(&sel) {
            self.select_row(sel.clone(), egui::Modifiers::default(), cx.row_sels);
        }
        egui::Popup::context_menu(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if !cx.group_list.is_empty() {
                    ui.menu_button(t!("ps_editor.layers_panel.move_to_group"), |ui| {
                        for (uid, name) in cx.group_list {
                            if ui.button(format!("📁 {name}")).clicked() {
                                actions.group_op = Some(GroupOp::MoveTo(uid.clone()));
                                egui::Popup::close_all(ui.ctx());
                            }
                        }
                    });
                }
                if ui
                    .button(t!("ps_editor.layers_panel.create_group_from_selection"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::NewFromSelection);
                    egui::Popup::close_all(ui.ctx());
                }
                if ui
                    .button(t!("ps_editor.layers_panel.remove_from_group"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::Ungroup);
                    egui::Popup::close_all(ui.ctx());
                }
            });
    }

    /// Applies a row click to the panel selection: plain = replace, Ctrl/Cmd = toggle, Shift = range
    /// over the displayed selectable rows (`row_sels`) from the anchor.
    ///
    /// A BASE row is the one exception: whatever the modifiers, it becomes a SOLO primary and the
    /// multi-selection is cleared. Base layers are structurally locked, so letting one join a
    /// Ctrl/Shift selection would hand it to `apply_group_op` (which reads `panel_selection`).
    /// `selectable_row_order` also keeps base rows out of `row_sels`, so a Shift range can never
    /// sweep one up; this guard is the second half of the same lock, at the mutation site.
    pub(crate) fn select_row(&mut self, sel: RowSel, mods: egui::Modifiers, row_sels: &[RowSel]) {
        if sel.is_base() {
            self.panel_selection.clear();
            self.panel_anchor = None;
            self.panel_primary = Some(sel);
            return;
        }
        if mods.shift
            && !row_sels.is_empty()
            && let Some(anchor) = self.panel_anchor.clone()
        {
            let a = row_sels.iter().position(|r| *r == anchor);
            let b = row_sels.iter().position(|r| *r == sel);
            if let (Some(a), Some(b)) = (a, b) {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                self.panel_selection = row_sels[lo..=hi].iter().cloned().collect();
                self.panel_primary = Some(sel);
                return;
            }
            // No usable anchor: fall through to a plain select.
        }
        if mods.command {
            if !self.panel_selection.remove(&sel) {
                self.panel_selection.insert(sel.clone());
            }
            self.panel_anchor = Some(sel.clone());
            self.panel_primary = Some(sel);
            return;
        }
        self.panel_selection.clear();
        self.panel_selection.insert(sel.clone());
        self.panel_anchor = Some(sel.clone());
        self.panel_primary = Some(sel);
    }

    /// The panel row naming the stack's current active layer (`LayerStack::active_id`), or `None`
    /// when no page is loaded. The variant follows the layer's kind, so a base layer yields
    /// `RowSel::Base` and a user raster yields `RowSel::Raster`.
    pub(crate) fn active_row_sel(&self) -> Option<RowSel> {
        let stack = self.stack.as_ref()?;
        let active = stack.active_id();
        let layer = stack.layer(active)?;
        Some(if layer.kind.is_base() {
            RowSel::Base(active)
        } else {
            RowSel::Raster(active)
        })
    }

    /// Whether `sel` still names a row the current tree would render.
    ///
    /// The kind must match too, not just the id: a `RowSel::Raster` that now resolves to a base
    /// layer (or the reverse) would drive `draw_active_controls` into the wrong arm.
    pub(crate) fn row_exists(&self, sel: &RowSel) -> bool {
        let Some(stack) = self.stack.as_ref() else {
            return false;
        };
        match sel {
            RowSel::Base(id) => stack.layer(*id).is_some_and(|l| l.kind.is_base()),
            RowSel::Raster(id) => stack.layer(*id).is_some_and(|l| !l.kind.is_base()),
            RowSel::Text(uid) => self.text_layers.iter().any(|t| t.uid() == uid),
            // A group needs BOTH halves: `LayerStack` may still hold a group whose last member was
            // deleted, and `tree::build_unified_tree` brackets a header around a run of member
            // leaves — so an emptied group renders no row at all.
            RowSel::Group(uid) => {
                stack.group_by_uid(uid).is_some() && self.group_has_members(stack, uid)
            }
        }
    }

    /// Whether group `uid` still owns at least one leaf — a non-base raster or a text overlay.
    ///
    /// Mirrors the membership `tree::build_unified_tree` walks, so `row_exists` and the rendered
    /// tree cannot disagree about whether a group has a row.
    pub(crate) fn group_has_members(&self, stack: &layers::LayerStack, uid: &str) -> bool {
        stack
            .layers()
            .iter()
            .any(|l| !l.kind.is_base() && stack.layer_group_uid(l.id).as_deref() == Some(uid))
            || self.text_layers.iter().any(|t| t.group_uid.as_deref() == Some(uid))
    }

    /// Re-establishes the panel invariant: **while a page is loaded, `panel_primary` is always
    /// `Some` and always names a row that exists in the current tree.**
    ///
    /// A `None` or dangling primary is re-pointed at `LayerStack::active_id()`, which the stack
    /// itself keeps valid (`LayerStack::new` starts at `Клин`, `remove_layer` falls back to the
    /// topmost survivor and finally to `Клин`). So a freshly loaded page shows `Клин` as the active
    /// layer, and deleting/merging away the active raster moves the highlight to the stack's
    /// fallback instead of leaving a strip that renders nothing at all. Stale rows are dropped from
    /// `panel_selection` at the same time so the multi-selection cannot outlive its rows either.
    ///
    /// No-op when no page is loaded (`stack == None`): there is no row to point at.
    ///
    /// This is view state, so nothing here is recorded in the undo history.
    pub(crate) fn normalize_panel_primary(&mut self) {
        if self.stack.is_none() {
            return;
        }
        let fallback = self.active_row_sel();
        // Drop rows that no longer exist from the multi-selection (cheap: the panel is small).
        let stale: Vec<RowSel> = self
            .panel_selection
            .iter()
            .filter(|s| !self.row_exists(s))
            .cloned()
            .collect();
        for sel in stale {
            self.panel_selection.remove(&sel);
        }
        if self.panel_anchor.as_ref().is_some_and(|s| !self.row_exists(s)) {
            self.panel_anchor = None;
        }
        if self
            .panel_primary
            .as_ref()
            .is_some_and(|s| self.row_exists(s))
        {
            return;
        }
        self.panel_primary = fallback;
    }

    /// Controls strip for the active row (`panel_primary`): opacity / merge / delete / fx for a
    /// raster, rasterize / ▲▼ for a text, opacity / delete for a group, and — for a structurally
    /// locked base layer — only its name plus the view-only opacity.
    ///
    /// The "select a layer" hint is a defensive fallback: while a page is loaded `panel_primary` is
    /// always `Some` (see the field's invariant), so it is reachable only before the first load.
    pub(crate) fn draw_active_controls(&self, ui: &mut egui::Ui, actions: &mut PanelActions) {
        let Some(primary) = self.panel_primary.clone() else {
            ui.label(t!("ps_editor.active_controls.select_layer_hint"));
            return;
        };
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        match primary {
            RowSel::Base(id) => {
                // Structurally locked: NO delete / merge / bake / fx / ▲▼ here. `Исходник` and
                // `Клин` are never removed, reordered, grouped or transformed.
                let Some(layer) = stack.layer(id).filter(|l| l.kind.is_base()) else {
                    return;
                };
                ui.label(format!("▦ {} 🔒", layer.name))
                    .on_hover_text(t!("ps_editor.active_controls.base_locked_tooltip"));
                // Opacity is genuinely honoured for base layers by the composite
                // (`draw_composite` → `LayerStack::layer_opacity`), and — like base-layer
                // visibility — it is VIEW-ONLY session state: base layers are not doc nodes and are
                // not persisted, so nothing is routed and nothing is recorded in the history
                // (`raster_uid` filters to `LayerKind::Raster`, so the opacity gesture records
                // nothing for a base id).
                let mut opacity = layer.opacity;
                if ui
                    .add(
                        ms_widgets::WheelSlider::new(&mut opacity, 0.0..=1.0)
                            .text(t!("ps_editor.active_controls.opacity_label")),
                    )
                    .changed()
                {
                    actions.opacity_raster = Some((id, opacity));
                }
            }
            RowSel::Raster(id) => {
                // The base filter is repeated at this mutation site on purpose (same habit as
                // `merge_down`): this arm exposes delete / merge / bake / fx / ▲▼, none of which a
                // base layer may ever reach, and that must not depend only on `build_panel_rows`
                // and `normalize_panel_primary` agreeing on which variant to construct.
                let Some(layer) = stack.layer(id).filter(|l| !l.kind.is_base()) else {
                    return;
                };
                ui.label(format!("▦ {}", layer.name));
                let mut opacity = layer.opacity;
                if ui
                    .add(
                        ms_widgets::WheelSlider::new(&mut opacity, 0.0..=1.0)
                            .text(t!("ps_editor.active_controls.opacity_label")),
                    )
                    .changed()
                {
                    actions.opacity_raster = Some((id, opacity));
                }
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("▲").small()).clicked() {
                        actions.move_band = Some((RowSel::Raster(id), true));
                    }
                    if ui.add(egui::Button::new("▼").small()).clicked() {
                        actions.move_band = Some((RowSel::Raster(id), false));
                    }
                    if ui.add(egui::Button::new("🗑").small()).clicked() {
                        actions.remove_raster = Some(id);
                    }
                    let mergeable = self.is_mergeable(id);
                    if ui
                        .add_enabled(mergeable, egui::Button::new("⤓").small())
                        .on_hover_text(t!("ps_editor.active_controls.merge_down"))
                        .clicked()
                    {
                        actions.merge_req = Some(id);
                    }
                    if ui
                        .add(egui::Button::new("fx").small())
                        .on_hover_text(t!("ps_editor.active_controls.effects_rasterize"))
                        .clicked()
                    {
                        actions.open_effects = Some(id);
                    }
                    // Bake a raster that is showing a non-destructive effects chain: flatten the
                    // render into the base pixels and clear the chain so it becomes directly editable.
                    if !layer.effects.is_empty()
                        && ui
                            .add(
                                egui::Button::new(t!("ps_editor.active_controls.bake_button"))
                                    .small(),
                            )
                            .on_hover_text(t!("ps_editor.active_controls.bake_tooltip"))
                            .clicked()
                    {
                        actions.bake_req = Some(id);
                    }
                });
            }
            RowSel::Text(uid) => {
                let Some((index, text)) = self
                    .text_layers
                    .iter()
                    .enumerate()
                    .find(|(_, t)| t.uid == uid)
                else {
                    return;
                };
                ui.label(format!("🅣 {}", text.name));
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("⊞").small())
                        .on_hover_text(t!("ps_editor.active_controls.bake_into_layer"))
                        .clicked()
                    {
                        actions.text_op = Some((index, TextLayerOp::Rasterize));
                    }
                    // Every text is pinned-with-explicit-Z now (fully-manual unified Z), so the ⬆/⬇
                    // band-move is ALWAYS available — same path as rasters (`move_band` → `move_band_one`
                    // → structural band-order job + `doc.set_z_order`), so the typing tab reflects it live.
                    if ui.add(egui::Button::new("▲").small()).clicked() {
                        actions.move_band = Some((RowSel::Text(uid.clone()), true));
                    }
                    if ui.add(egui::Button::new("▼").small()).clicked() {
                        actions.move_band = Some((RowSel::Text(uid.clone()), false));
                    }
                });
            }
            RowSel::Group(uid) => {
                let Some(group) = stack.group_by_uid(&uid) else {
                    return;
                };
                ui.label(format!("📁 {}", group.name));
                let mut opacity = group.opacity;
                if ui
                    .add(
                        ms_widgets::WheelSlider::new(&mut opacity, 0.0..=1.0)
                            .text(t!("ps_editor.active_controls.opacity_label")),
                    )
                    .changed()
                {
                    actions.group_op = Some(GroupOp::GroupOpacity(uid.clone(), opacity));
                }
                if ui
                    .add(
                        egui::Button::new(t!("ps_editor.layers_panel.delete_group_button")).small(),
                    )
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::DeleteGroup(uid));
                }
            }
        }
    }

    /// Every legal merge-down TARGET as `(uid, band_z)` in stack order: the user rasters plus the
    /// `Клин` base layer, which sits below all of them.
    ///
    /// A raster's `band_z` is its `Band::Raster` Z from `self.bands` (or the past-the-top fallback
    /// for a raster without a band, mirroring `draw_composite`), SHIFTED UP BY ONE so that `Клин`
    /// can occupy the synthetic Z 0 strictly beneath every raster — the shift preserves the rasters'
    /// relative order exactly, and keeps the whole axis in `u32`. `Исходник` is excluded: it is
    /// immutable and must never become a merge target. The order within the Vec is the stack order,
    /// used only as the stable tiebreak.
    pub(crate) fn merge_candidates_by_band_z(&self) -> Vec<(String, u32)> {
        let Some(stack) = self.stack.as_ref() else {
            return Vec::new();
        };
        stack
            .layers()
            .iter()
            .filter(|l| l.kind != LayerKind::Source)
            .map(|l| {
                let uid = l.uid.to_string();
                let z = match l.kind {
                    // Reserved bottom slot: Клин is composited under every raster.
                    LayerKind::Clean => 0,
                    LayerKind::Raster => {
                        ordering::band_z(&self.bands, CompositeKey::Raster { uid: &uid })
                            .saturating_add(1)
                    }
                    // Filtered out above.
                    LayerKind::Source => u32::MAX,
                };
                (uid, z)
            })
            .collect()
    }

    /// The layer directly beneath `id` on the unified band-Z axis (the visually-below layer the user
    /// sees in the composite), or `None` if `id` has nothing below it or is itself a base layer.
    ///
    /// The result may be the `Клин` base layer (the bottom-most raster merges INTO it); it is never
    /// `Исходник`. `id` itself must be a non-base layer: base layers are never the UPPER participant
    /// of a merge, which is also why the panel gives them no control strip.
    /// Band-Z based, so a manual reorder picks the correct pair (not the stack neighbor).
    pub(crate) fn raster_below_uid(&self, id: LayerId) -> Option<String> {
        let stack = self.stack.as_ref()?;
        let target = stack.layer(id).filter(|l| !l.kind.is_base())?;
        let target_uid = target.uid.to_string();
        raster_below_by_band_z(&self.merge_candidates_by_band_z(), &target_uid)
    }

    /// A raster can be merged down when there is another merge target directly beneath it on the
    /// unified band-Z axis — another raster, or `Клин` for the bottom-most one. Base layers are
    /// never mergeable themselves, and `Исходник` is never a target.
    pub(crate) fn is_mergeable(&self, id: LayerId) -> bool {
        self.raster_below_uid(id).is_some()
    }

    /// Applies the panel's deferred actions (everything that needs `&mut self` / `project`).
    pub(crate) fn apply_panel_actions(&mut self, actions: PanelActions, project: &ProjectData) {
        let page_idx = self.active_page_idx;
        if actions.add_layer
            && let Some(stack) = self.stack.as_mut()
        {
            // Add the local layer first (it is `pixels_dirty`, so a re-projection won't clobber its
            // empty pixels), then mirror it as a doc node so cross-tab reads see it.
            let id = stack.add_raster_layer();
            ms_log::trace_log!(
                cat::PS_EDITOR,
                "panel add_layer id={} page={:?}",
                id,
                page_idx
            );
            self.panel_primary = Some(RowSel::Raster(id));
            if let (Some(page_idx), Some(node)) = (
                page_idx,
                self.stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(layer_to_raster_node),
            ) {
                self.route_to_doc(page_idx, project, |doc| {
                    doc.add_node(page_idx, node);
                });
                // Record the ADD (observer style — the layer is already live). Undo → inverse
                // (Removed) deletes it; redo re-adds at the captured Z. Read the Z back from the
                // re-projected bands (the doc assigned it on top).
                if let Some(layer) = self.stack.as_ref().and_then(|s| s.layer(id)).cloned() {
                    let z = self.raster_band_z(&layer.uid.to_string());
                    self.history.record(PsEditOp::LayerLifecycle {
                        page_idx,
                        layer: Box::new(layer),
                        z,
                        dir: LifecycleDir::Added,
                    });
                }
            }
        }
        if actions.new_empty_group
            && let Some(stack) = self.stack.as_mut()
        {
            let n = stack.groups().len() + 1;
            // Persisted group name (round-trips to `layers.json`); stable literal, not
            // localized. See dev-docs/i18n_exclusions.md §A (persisted layer/group names).
            let gid = stack.add_group(format!("Группа {n}"));
            ms_log::trace_log!(cat::PS_EDITOR, "panel new_empty_group gid={}", gid);
        }
        if let Some(id) = actions.set_active_raster
            && let Some(stack) = self.stack.as_mut()
        {
            ms_log::trace_log!(cat::PS_EDITOR, "panel set_active_raster id={}", id);
            // Active selection is LOCAL-only (not a doc model field): keep it on the stack.
            stack.set_active(id);
        }
        // After the active layer/primary is updated (and the `&mut self.stack` borrow above is
        // dropped), show the selected layer's marquee immediately. `select_active_layer_fully`
        // borrows `self.stack`/`self.text_layers` and writes `self.selection`, so it must run
        // OUTSIDE the `self.stack.as_mut()` scope.
        if actions.request_select_active {
            self.select_active_layer_fully();
        }
        if let Some(id) = actions.toggle_visible_raster {
            let is_base = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .is_some_and(|l| l.kind.is_base());
            if is_base {
                // Base-layer visibility is VIEW-ONLY session state: `Исходник`/`Клин` have no doc
                // node to patch and are deliberately never persisted (`persist_current_page`
                // filters to `LayerKind::Raster`), so there is nothing to route and nothing to
                // record in the history. `draw_composite` already honours `Layer::visible` for base
                // layers, which makes a plain stack write sufficient.
                if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id)) {
                    layer.visible = !layer.visible;
                    ms_log::trace_log!(
                        cat::PS_EDITOR,
                        "panel toggle_visible_base id={} visible={}",
                        id,
                        layer.visible
                    );
                }
            } else if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                let new_visible = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .is_some_and(|l| !l.visible);
                ms_log::trace_log!(
                    cat::PS_EDITOR,
                    "panel toggle_visible_raster id={} visible={}",
                    id,
                    new_visible
                );
                if !self.route_to_doc(page_idx, project, |doc| {
                    doc.set_visibility(page_idx, &uid, new_visible);
                }) && let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id))
                {
                    layer.visible = new_visible;
                }
                // Record the toggle (a toggle always changes value → always record).
                self.history.record(PsEditOp::FieldPatch {
                    page_idx,
                    layer_uid: uid,
                    field: LayerFieldPatch::Visibility {
                        before: !new_visible,
                        after: new_visible,
                    },
                });
            }
        }
        if let Some(i) = actions.toggle_visible_text
            && let Some((uid, before)) = self.text_layers.get(i).map(|t| (t.uid.clone(), t.visible))
        {
            ms_log::trace_log!(
                cat::PS_EDITOR,
                "panel toggle_visible_text index={} visible={}",
                i,
                !before
            );
            // Doc-level, like a raster eye: the typing canvas, its export and Save omit a hidden text
            // node too ("what you see is what you save"). The text payload carries `visible`, so the
            // page's text save persists it.
            match page_idx {
                Some(page_idx) => {
                    if self.write_text_visibility(page_idx, &uid, !before) {
                        self.flush_text_page(page_idx, project);
                    }
                    self.history.record(PsEditOp::TextVisibility {
                        page_idx,
                        text_uid: uid,
                        before,
                        after: !before,
                    });
                }
                None => {
                    if let Some(layer) = self.text_layers.get_mut(i) {
                        layer.visible = !before;
                    }
                }
            }
        }
        if let Some((id, value)) = actions.opacity_raster {
            // Live slider: fires only on actual value change (drag steps), not every idle frame.
            ms_log::trace_log!(
                cat::PS_EDITOR,
                "panel opacity_raster id={} value={:.3}",
                id,
                value
            );
            // Snapshot the pre-drag opacity ONCE per gesture (the stack still holds it before this
            // frame's apply), so the whole drag records a single undo step (see `opacity_gesture`).
            if self.opacity_gesture.is_none()
                && let Some(before) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(|l| l.opacity)
            {
                self.opacity_gesture = Some((id, before));
            }
            // Live slider: mutate the doc node in memory + re-project, but don't flush each frame
            // (persisted on page-leave). Falls back to a local edit if no doc page is resident.
            if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                if !self.edit_doc_node(page_idx, |doc| {
                    doc.set_opacity(page_idx, &uid, value);
                }) && let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id))
                {
                    layer.opacity = value;
                }
            } else if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id)) {
                layer.opacity = value;
            }
        } else if let Some((id, before)) = self.opacity_gesture.take() {
            // First frame with no further opacity change ⇒ the drag ended. Record one `FieldPatch`
            // for the whole gesture if the value actually moved.
            if let (Some(page_idx), Some(uid), Some(after)) = (
                page_idx,
                self.raster_uid(id),
                self.stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(|l| l.opacity),
            ) && (after - before).abs() > f32::EPSILON
            {
                self.history.record(PsEditOp::FieldPatch {
                    page_idx,
                    layer_uid: uid,
                    field: LayerFieldPatch::Opacity { before, after },
                });
            }
        }
        if let Some(id) = actions.remove_raster {
            ms_log::trace_log!(
                cat::PS_EDITOR,
                "panel remove_raster id={} page={:?}",
                id,
                page_idx
            );
            // Capture the FULL layer (with pixels) + its Z BEFORE removal so an undo can re-add it.
            let captured = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .filter(|l| l.kind == LayerKind::Raster)
                .cloned();
            let captured_z = captured
                .as_ref()
                .map(|l| self.raster_band_z(&l.uid.to_string()));
            // Record the deletion (so the manifest save drops it — `flush_page`/`save_page_rasters`
            // preserve unowned rasters, so a removal must be explicit), remove it from the doc in
            // memory + re-project, then persist via `persist_current_page` (which carries the removed
            // uid and bumps the revision).
            self.record_raster_deletion(id);
            if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                self.edit_doc_node(page_idx, |doc| {
                    doc.remove_node(page_idx, &uid);
                });
            } else if let Some(stack) = self.stack.as_mut() {
                stack.remove_layer(id);
            }
            self.persist_current_page(project);
            // The deleted layer may have been the primary row. Re-point it at the stack's own
            // fallback (`LayerStack::remove_layer` already moved `active` to the topmost survivor,
            // finally `Клин`) so the controls strip keeps showing a layer. Needed here as well as in
            // `sync_view_from_doc`, because the no-doc fallback path above removes the layer from
            // the stack directly and never reaches a projection.
            self.normalize_panel_primary();
            // Record the DELETE (observer style). Undo → inverse (Added) re-adds it at its prior Z.
            if let (Some(page_idx), Some(layer), Some(z)) = (page_idx, captured, captured_z) {
                self.history.record(PsEditOp::LayerLifecycle {
                    page_idx,
                    layer: Box::new(layer),
                    z,
                    dir: LifecycleDir::Removed,
                });
            }
        }
        if let Some(id) = actions.merge_req {
            ms_log::trace_log!(cat::PS_EDITOR, "panel merge_down id={}", id);
            self.merge_down(id, project);
        }
        if let Some(id) = actions.bake_req {
            ms_log::trace_log!(cat::PS_EDITOR, "panel bake_raster id={}", id);
            self.bake_raster(id, project);
        }
        if let Some(id) = actions.open_effects {
            ms_log::trace_log!(cat::PS_EDITOR, "panel open_effects id={}", id);
            // Seed the editor with the layer's current (non-destructive) chain so effects can be
            // tweaked or cleared rather than always starting blank.
            let seed = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .filter(|l| !l.effects.is_empty())
                .map(|l| serde_json::to_string_pretty(&l.effects).unwrap_or_default())
                .unwrap_or_default();
            self.effects_editor = Some((id, seed));
        }
        if let Some((index, op)) = actions.text_op {
            ms_log::trace_log!(cat::PS_EDITOR, "panel text_op index={} op={:?}", index, op);
            self.apply_text_layer_op(index, op, project);
        }
        if let Some((sel, up)) = actions.move_band {
            ms_log::trace_log!(cat::PS_EDITOR, "panel move_band sel={:?} up={}", sel, up);
            self.move_band_one(sel, up, project);
        }
        // A frame without a group-opacity change ends an open group-opacity drag (one undo step).
        if !matches!(actions.group_op, Some(GroupOp::GroupOpacity(..))) {
            self.finish_group_opacity_gesture();
        }
        if let Some(op) = actions.group_op {
            ms_log::trace_log!(cat::PS_EDITOR, "panel group_op op={:?}", op);
            self.apply_group_op(op, project);
        }
    }
}
