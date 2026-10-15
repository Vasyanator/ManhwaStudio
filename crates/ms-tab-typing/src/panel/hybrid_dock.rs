/*
File: panel/hybrid_dock.rs

Purpose:
The «Гибридный шрифт» dock tab of the «Текст» program tab (`typing.hybrid_font`, declared in
`tab.rs`): the shared hybrid editor (`crate::hybrid_editor`) hosted as a panel-dock tab that
edits the AD-HOC recipe of the ACTIVE panel — the create panel's current parameters, or the
edited layer in the edit panel (`dev-docs/hybrid_fonts_plan.md` §3.5, decision D5).

Key structures:
- `HybridDockState`: open flag, the bound editor, the source catalog.
- `HybridDockBinding`: which panel / layer the editor edits; a change rebuilds the editor.

Key functions:
- `hybrid_font_tab_visible` (pure rule: `hybrid_dock_visible`), `sync_hybrid_dock` (per-frame
  upkeep from `begin_frame`), `drain_hybrid_dock_request` (the row's button),
  `draw_hybrid_font_tab_body` (the tab body + «Закрыть»), `apply_committed_hybrid_recipe`.
- `preview_text_from_layer`: the layer text without inline markup, the editor's sample.

Notes:
The canvas changes ONLY on «Применить»: draft edits recompute the editor's own preview glyphs
(its worker), and only a finished full-face commit is installed into the panel
(`install_adhoc_recipe`) — then the create panel re-renders its preview and the edit panel
emits the same edit request as any parameter change. A commit whose panel / layer is no longer
the one the editor was opened for is dropped with a warning, never applied elsewhere.
GUI thread only; nothing here blocks: the catalog build clones list entries (no I/O), the
editor's preview and compile run on its own workers.
*/

use super::*;
use super::custom_font_select::HybridRowStatus;
use crate::hybrid_editor::{CommitButton, HybridEditorCx, HybridEditorOutcome, HybridEditorState, SourceCatalog, draw_hybrid_editor};

/// Salt of the dock editor's widget ids (one editor per panel mode, never two at once).
const HYBRID_DOCK_EDITOR_ID_SALT: &str = "typing.hybrid_font.dock_editor";

/// Which panel, and in the edit panel which layer, the dock editor edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HybridDockBinding {
    /// The panel whose ad-hoc recipe is edited.
    mode: TypingTopPanelMode,
    /// The edited layer (edit panel only; `None` in create mode).
    edit_target: Option<TypingEditTarget>,
}

/// The editor of one binding.
#[derive(Debug)]
struct BoundEditor {
    /// What it edits.
    binding: HybridDockBinding,
    /// The panel recipe it was opened on (or last committed); a different panel recipe
    /// rebuilds a clean, idle editor.
    baseline: Option<Arc<CustomFontRecipe>>,
    /// The shared editor body's state.
    state: HybridEditorState,
}

/// The «Гибридный шрифт» dock tab's state (owned by `TypingTopPanelState`).
#[derive(Debug, Default)]
pub(super) struct HybridDockState {
    /// Toggled by «Настроить гибридный шрифт», cleared by «Закрыть». The tab is drawn only
    /// while this holds AND the active panel's font is the ad-hoc entry.
    open: bool,
    /// The editor; exists only while the tab is visible.
    editor: Option<BoundEditor>,
    /// The sources offered to the editor; rebuilt when the active panel's provider changes
    /// (a font reload), dropped with the editor.
    catalog: Option<Arc<SourceCatalog>>,
}

/// Whether the dock tab is drawn: the user opened it and the active panel's selected font is
/// the ad-hoc entry (another font has no ad-hoc recipe to edit).
#[must_use]
pub(super) fn hybrid_dock_visible(open: bool, active_font_is_adhoc: bool) -> bool {
    open && active_font_is_adhoc
}

/// The editor's preview sample taken from a layer text: inline markup the panel understands
/// (`<b>`, `<font=…>`, `<m …>`, …) is removed — it is not text the reader sees — and line
/// breaks become spaces. Unknown `<…>` sequences stay, as the renderer draws them literally.
/// The editor itself cuts the result to its maximum length.
#[must_use]
pub(super) fn preview_text_from_layer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        match tail.find('>') {
            Some(end) => {
                let raw = &tail[1..end];
                if parse_opening_inline_tag(raw).is_none() && parse_closing_inline_tag(raw).is_none() {
                    out.push_str(&tail[..=end]);
                }
                rest = &tail[end + 1..];
            }
            None => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.split(['\n', '\r']).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ")
}

impl TypingTopPanelState {
    /// The panel the dock edits: the one of the current mode.
    fn hybrid_panel(&self) -> &TypingCreatePanelState {
        match self.mode {
            TypingTopPanelMode::CreateText => &self.create_panel,
            TypingTopPanelMode::EditText => &self.edit_panel,
        }
    }

    /// What the dock editor is bound to this frame.
    pub(super) fn hybrid_binding(&self) -> HybridDockBinding {
        let edit_target = match self.mode {
            TypingTopPanelMode::CreateText => None,
            TypingTopPanelMode::EditText => self.edit_target.clone(),
        };
        HybridDockBinding { mode: self.mode, edit_target }
    }

    /// Whether the «Гибридный шрифт» dock tab is drawn this frame (`hybrid_dock_visible`).
    #[must_use]
    pub(crate) fn hybrid_font_tab_visible(&self) -> bool {
        hybrid_dock_visible(self.hybrid_dock.open, self.hybrid_panel().selected_font_is_adhoc())
    }

    /// Per-frame upkeep, run from `begin_frame` before the dock: drops the editor while the
    /// tab is hidden, (re)builds the catalog and the editor for the current binding, and
    /// pushes the commit status into the row under each panel's font combo.
    ///
    /// The editor is rebuilt when its binding changed (another panel or layer: an unapplied
    /// draft belongs to the layer it was made for and is discarded), or when it is clean and
    /// idle and the panel's recipe differs from the one it was opened on (a preset apply, a
    /// layer reload). A running commit is never interrupted by the second rule.
    pub(super) fn sync_hybrid_dock(&mut self) {
        if !self.hybrid_font_tab_visible() {
            self.hybrid_dock.editor = None;
            self.hybrid_dock.catalog = None;
        } else {
            let binding = self.hybrid_binding();
            // Borrow the panel through the field, not `hybrid_panel()`, so the dock state
            // (a sibling field) stays assignable below.
            let panel = match self.mode {
                TypingTopPanelMode::CreateText => &self.create_panel,
                TypingTopPanelMode::EditText => &self.edit_panel,
            };
            let resolver = panel.source_resolver();
            let catalog_current = self.hybrid_dock.catalog.as_ref().is_some_and(|catalog| Arc::ptr_eq(catalog.resolver(), &resolver));
            if !catalog_current {
                self.hybrid_dock.catalog = Some(Arc::new(SourceCatalog::from_entries(&panel.fonts, resolver)));
            }
            let panel_recipe = panel.custom_font.adhoc_recipe.clone();
            let rebuild = self.hybrid_dock.editor.as_ref().is_none_or(|bound| {
                bound.binding != binding
                    || (!bound.state.is_committing() && !bound.state.is_dirty() && bound.baseline != panel_recipe)
            });
            if rebuild {
                let id = egui::Id::new((HYBRID_DOCK_EDITOR_ID_SALT, binding.mode == TypingTopPanelMode::CreateText));
                let mut state = match panel_recipe.as_deref() {
                    Some(recipe) => HybridEditorState::from_recipe(recipe, id),
                    None => HybridEditorState::new_draft(id),
                };
                let sample = preview_text_from_layer(&panel.effective_render_text());
                if !sample.trim().is_empty() {
                    state.set_preview_text(&sample);
                }
                self.hybrid_dock.editor = Some(BoundEditor { binding, baseline: panel_recipe, state });
            }
        }
        let (status, bound_mode) = self.hybrid_dock.editor.as_ref().map_or((HybridRowStatus::Idle, None), |bound| {
            let status = if bound.state.is_committing() {
                HybridRowStatus::Compiling(bound.state.commit_progress())
            } else if bound.state.commit_failed() {
                HybridRowStatus::Failed
            } else {
                HybridRowStatus::Idle
            };
            (status, Some(bound.binding.mode))
        });
        self.create_panel.custom_font.row_status =
            if bound_mode == Some(TypingTopPanelMode::CreateText) { status } else { HybridRowStatus::Idle };
        self.edit_panel.custom_font.row_status =
            if bound_mode == Some(TypingTopPanelMode::EditText) { status } else { HybridRowStatus::Idle };
    }

    /// Drains «Настроить гибридный шрифт» from both panels (after the params body drew) and
    /// toggles the dock tab. Closing drops the editor (and its unapplied draft).
    pub(super) fn drain_hybrid_dock_request(&mut self) {
        // Both are drained every frame so a click can never linger in the other panel.
        let create = self.create_panel.take_hybrid_configure_request();
        let edit = self.edit_panel.take_hybrid_configure_request();
        if create || edit {
            self.hybrid_dock.open = !self.hybrid_dock.open;
            if !self.hybrid_dock.open {
                self.hybrid_dock.editor = None;
            }
        }
    }

    /// Body of the «Гибридный шрифт» dock tab (`typing.hybrid_font`): the shared editor with
    /// «Применить», then «Закрыть». A finished commit is installed into the bound panel
    /// (`apply_committed_hybrid_recipe`).
    pub(crate) fn draw_hybrid_font_tab_body(&mut self, ui: &mut egui::Ui) {
        let (Some(bound), Some(catalog)) = (self.hybrid_dock.editor.as_mut(), self.hybrid_dock.catalog.as_ref()) else {
            return;
        };
        let outcome =
            draw_hybrid_editor(ui, &mut bound.state, &HybridEditorCx { catalog: catalog.as_ref(), commit: CommitButton::Apply });
        let committed = match outcome {
            HybridEditorOutcome::Committed(committed) => {
                bound.baseline = Some(Arc::clone(&committed.recipe));
                Some((bound.binding.clone(), committed.recipe))
            }
            // The failure is logged and shown by the editor; the row reads it next frame.
            HybridEditorOutcome::Idle | HybridEditorOutcome::CommitFailed => None,
        };
        ui.separator();
        let close = ui.button(t!("typing.hybrid_font.close_button")).clicked();
        if let Some((binding, recipe)) = committed {
            self.apply_committed_hybrid_recipe(&binding, recipe);
        }
        if close {
            self.hybrid_dock.open = false;
            self.hybrid_dock.editor = None;
        }
    }

    /// Installs a committed recipe into the panel `binding` names — the ONLY way the dock
    /// changes a layer or the create parameters — and propagates it like a parameter edit:
    /// the create panel stores its parameter snapshot and re-renders its preview, the edit
    /// panel emits the layer's edit request. Dropped with a warning when the binding is no
    /// longer the current one (the commit finished after the user moved on).
    pub(super) fn apply_committed_hybrid_recipe(&mut self, binding: &HybridDockBinding, recipe: Arc<CustomFontRecipe>) {
        if *binding != self.hybrid_binding() {
            ms_log::runtime_log::log_warn(format!(
                "typing: a hybrid font finished building for a panel or layer that is no longer \
                 selected ({binding:?}); it is not applied. Recipe: {}",
                recipe.to_json()
            ));
            return;
        }
        match binding.mode {
            TypingTopPanelMode::CreateText => {
                self.create_panel.install_adhoc_recipe(Some(recipe));
                self.create_panel.store_current_params_snapshot();
                self.create_panel.queue_preview_render();
            }
            TypingTopPanelMode::EditText => {
                self.edit_panel.install_adhoc_recipe(Some(recipe));
                self.emit_edit_request();
            }
        }
    }
}
