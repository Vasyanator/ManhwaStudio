/*
File: crates/ms-tab-ps-editor/src/dock_panels.rs

Purpose:
The «PS редактор» panel-dock surface: the default dock arrangement and the per-tab body functions
the dock calls each frame.

Key functions:
- `ps_editor_default_dock_layout()` (pub, re-exported at the crate root): default arrangement of the
  seven dock tabs.
- `draw_*_tab_body()`: one body per dock tab, each borrowing `PsEditorDockCx` and forwarding to a
  `PsEditorTabState` method, the active tool's panel or the «Коррекция» body (`correction/`).
- `ps_editor_panels_menu()`: the «Панели…» visibility menu of the main tab.
- `draw_hotkey_rows_grid()`: two-column grid of localized hotkey rows.

Notes:
The tab ids, panel-visibility flags, size constants and the `PsEditorDockCx` /
`PsEditorPanelVisibility` types live in `lib.rs`; this file only consumes them.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// Default panel arrangement of the «PS редактор» program tab.
///
/// Two columns over the canvas. On the LEFT, «PS редактор» → «Инструменты» → «Выбранный инструмент»
/// → the active tool's own panel, which mirrors what the static layout showed as a top strip plus a
/// left toolbar. On the RIGHT, «Слои» → «Горячие клавиши» and «Слои» → «Коррекция», mirroring the
/// old right panel and keeping the left chain from stacking four VISIBLE panels deep on a short
/// window — the tool panel is drawn only while the active tool asks for it, which no shipped tool
/// does.
///
/// It must name EVERY `TabId` this program tab can declare: `panel_dock::persist` resolves a
/// stored tab key against the default layout's tab set, so a tab missing here would be dropped
/// from the user's arrangement on every load (`panel_dock/persist.rs::known_tabs`).
///
/// No size is pinned: each tab states its own `min_size` / `initial_size` per frame, and the
/// solver sizes a panel from what its content measured.
///
/// Used only when no layout exists yet for this program tab; a restored one always wins. Handed to
/// the app-owned dock state as a plain `fn` pointer, both when the persisted layouts are restored
/// before the first frame and by `ensure_default_layout` on every frame this tab draws — the state
/// keeps that pointer to serve the header's «Сбросить раскладку» item. A model refusal is logged
/// and skipped, never panicked on: the dock then creates a panel for the orphaned tab on its own,
/// which is a degraded arrangement rather than a lost tab.
#[must_use]
// `pub`, not crate-private: `studio.rs::restore_panel_dock` hands it to the app-owned dock
// state as a `fn` pointer, and it lives in another crate.
pub fn ps_editor_default_dock_layout() -> DockLayout {
    let mut layout = DockLayout::new();
    let main = PanelId::new(0);
    let tools = PanelId::new(1);
    let layers = PanelId::new(3);
    let panels = [
        // Insertion order IS anchor order: `insert_panel` rejects an anchor whose target does not
        // exist yet, so every chain root precedes its dependants.
        (
            main,
            vec![PS_EDITOR_MAIN_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            },
        ),
        (
            tools,
            vec![PS_EDITOR_TOOLS_TAB],
            PanelAnchor::Panel {
                target: main,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            PanelId::new(2),
            vec![PS_EDITOR_ACTIVE_TOOL_TAB],
            PanelAnchor::Panel {
                target: tools,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            layers,
            vec![PS_EDITOR_LAYERS_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0,
            },
        ),
        (
            PanelId::new(4),
            vec![PS_EDITOR_HOTKEYS_TAB],
            PanelAnchor::Panel {
                target: layers,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            PanelId::new(5),
            vec![PS_EDITOR_CORRECTION_TAB],
            // Anchored to «Горячие клавиши», NOT to «Слои». Two panels sharing one target, edge and
            // align solve to the SAME rect — `solver::place_outside` is a pure function of the
            // target rect and does no occupancy check — and the second one is then buried and
            // unreachable. That is the failure the solver's own «THE SIBLING CONTRACT» test
            // (`panel_dock/solver.rs`) exists to pin. Chaining continues the right-hand column
            // «Слои» -> «Горячие клавиши» -> «Коррекция».
            PanelAnchor::Panel {
                target: PanelId::new(4),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            PanelId::new(6),
            vec![PS_EDITOR_TOOL_PANEL_TAB],
            // Continues the LEFT chain under «Выбранный инструмент», the panel it is closest kin
            // to — both belong to whichever tool is active. It costs the default arrangement
            // nothing: no shipped tool answers `wants_main_panel`, so the panel is not drawn at
            // all until a region tool is added. Anchored to `active_tool`, not shared with it —
            // see the «Коррекция» comment above for why two panels may not share one target.
            PanelAnchor::Panel {
                target: PanelId::new(2),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
    ];
    for (id, tabs, anchor) in panels {
        let node = match PanelNode::new(id, HostId::MainWindow, tabs) {
            Ok(mut node) => {
                node.anchor = anchor;
                node
            }
            Err(error) => {
                ms_log::runtime_log::log_warn(format!(
                    "[ps_editor] default dock layout: could not build panel {id} ({error}); \
                     the dock will create one per orphaned tab on its own"
                ));
                continue;
            }
        };
        if let Err(error) = layout.insert_panel(node) {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] default dock layout: could not insert panel {id} ({error}); \
                 the dock will create one per orphaned tab on its own"
            ));
        }
    }
    layout
}

/// Draws the «PS редактор» tab body: the page/zoom strip plus the «Панели…» visibility menu.
///
/// The only body declared with `show_with_extras`, because it is the only one that WRITES the
/// dock's per-tab extra state: the five flags are pushed on every frame with the value the menu
/// currently shows, which is the usage `TabExtras::set_flag` is designed for — it stores nothing
/// while a flag equals its default and raises `changed` only when the content really moved.
pub(crate) fn draw_main_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>, extras: &mut TabExtras) {
    cx.tab.main_tab_contents(ui, cx.project);
    ui.separator();
    ps_editor_panels_menu(ui, &mut cx.visibility);
    for (key, _title, value) in cx.visibility.entries() {
        extras.set_flag(key, *value, PS_EDITOR_PANEL_VISIBLE_DEFAULT);
    }
}

/// The «Панели…» menu button: one checkbox per SECONDARY panel of this program tab.
///
/// «PS редактор» itself is never listed — the menu lives in it, so hiding it could not be undone.
///
/// The menu is configured `CloseOnClickOutside` instead of egui's default `CloseOnClick`, so the
/// user can toggle several panels in one visit; egui closes it on Esc or on a click elsewhere
/// (`egui-0.35.0/src/containers/popup.rs:77-91`). Ids come from the flag-key literals, never from
/// the localized captions (`egui-docs/05-ids-and-i18n.md` §2).
pub(crate) fn ps_editor_panels_menu(ui: &mut egui::Ui, visibility: &mut PsEditorPanelVisibility) {
    let (response, _inner) = egui::menu::MenuButton::new(t!("ps_editor.panels_menu.button"))
        .config(
            egui::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            for (key, title, value) in visibility.entries() {
                ui.push_id(key, |ui| ui.checkbox(value, title));
            }
        });
    response.on_hover_text(t!("ps_editor.panels_menu.tooltip"));
}

/// Draws the «Инструменты» tab body.
pub(crate) fn draw_tools_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.tools_tab_contents(ui, cx.project);
}

/// Draws the «Выбранный инструмент» tab body.
pub(crate) fn draw_active_tool_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.active_tool_tab_contents(ui);
}

/// Draws the «Горячие клавиши» tab body.
pub(crate) fn draw_hotkeys_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.hotkeys_tab_contents(ui);
}

/// Draws the ACTIVE TOOL's own dock panel body ([`PsTool::draw_main_panel`]).
///
/// Declared every frame and made visible only while the active tool answers
/// [`PsTool::wants_main_panel`], so a tool that owns no panel costs one declaration. The body may
/// mutate the TOOL and nothing else — it runs inside the dock frame, before the canvas exists this
/// frame, so a button here raises a flag the tool acts on in its next `draw_overlay_ui`.
pub(crate) fn draw_tool_panel_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    if let Some(tool) = cx.tab.tools.get_mut(cx.tab.active_tool_idx) {
        tool.draw_main_panel(ui);
    }
}

/// Draws the «Коррекция» tab body: the view-only colour correction of the canvas.
///
/// Mutating `cx.tab` here is sound and deliberate — a PS-editor body is lent the whole tab state
/// (`PsEditorDockCx`), and the correction is per-session view state exactly like `smoothing_enabled`.
/// The canvas reads it fresh every frame, so nothing has to be invalidated from here.
pub(crate) fn draw_correction_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    let unavailable = cx.tab.correction_availability.check(ui.ctx());
    correction::correction_panel_body(ui, &mut cx.tab.correction, unavailable);
}

/// Draws the «Слои» tab body, parking its deferred actions in the frame context.
///
/// The actions are NOT applied here: `apply_panel_actions` rewrites the layer stack, the shared
/// document and the on-disk manifest, and it must run after the dock frame has finished — the same
/// place the static right panel applied them from.
pub(crate) fn draw_layers_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.panel_actions = cx.tab.layers_panel_body(ui);
}

/// Lays a shortcut inventory out as a two-column grid: the action, then its keys in monospace.
///
/// `id_salt` must be a stable literal — two grids in one panel need distinct ids, and a localized
/// caption must never become one (`egui-docs/05-ids-and-i18n.md` §2).
///
/// Deliberately a LOCAL renderer rather than a reuse of the canvas' equivalent
/// (`CanvasScene::draw_hint_rows_grid`): that one is private to the canvas impl, this tab is not a
/// canvas tab, and lifting it into a shared widget would be a refactor of another subsystem for no
/// gain here. The duplication is a few lines and is recorded in this module's `MODULE_README.md`.
pub(crate) fn draw_hotkey_rows_grid(ui: &mut egui::Ui, id_salt: &'static str, rows: &[tools::PsHotkeyRow]) {
    egui::Grid::new(id_salt)
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for row in rows {
                ui.label(&row.action);
                ui.monospace(&row.keys);
                ui.end_row();
            }
        });
}
