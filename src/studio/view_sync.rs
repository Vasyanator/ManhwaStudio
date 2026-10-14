/*
File: src/studio/view_sync.rs

Purpose:
Shared view synchronization between the studio's "view" tabs: the three canvas tabs
(Translation, Cleaning, Typing — each a `CanvasView`) and the PS editor (own camera). Keeps the
shared viewport snapshot, the tab that owns it, the shared page and its page-local center, and
the previously active view tab, and applies / publishes them around the active tab's draw.

Key structures:
- `ViewSync`: the shared view state (owned by `StudioApp`).
- `ViewTabs`: the tab states `ViewSync` reads and writes for one call, borrowed from `Tabs`
  (`Tabs::view_tabs`; tests build it from default tab states).
- `CanvasSlot` / `canvas_slot`: the ONE mapping from an `AppTab` to the canvas it draws;
  `for_each_canvas_mut`: the fan-out over all three canvases.

Key functions:
- `ViewSync::before_draw`: tab-leave flushes (PS, Typing, the bubble edits of every canvas the
  active tab does not draw), then the snapshot / page apply.
- `ViewSync::after_draw`: publishes the active view tab's viewport, page and center.
- `ViewSync::open_page`: explicit page focus that the next `before_draw` must not overwrite.

Notes:
Owned struct with a narrow API: it never reaches `StudioApp`'s fields. `active_tab` stays on
`StudioApp` and is passed in. Per-frame order (in `StudioApp::ui`): `before_draw` right before the
active tab draws, `after_draw` at the end of the frame.
*/

use crate::canvas::{CanvasView, CanvasViewportSnapshot};
use crate::project::ProjectData;
use crate::tabs::AppTab;
use crate::tabs::cleaning::CleaningTabState;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::{TypingSaveFlushReason, TypingTabState};
use eframe::egui;

/// Which `CanvasView` a canvas tab draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CanvasSlot {
    /// The canvas owned by `TranslationTab` (`canvas_mut`), drawn by the Translation tab.
    Translation,
    /// The canvas owned by `CleaningTabState`.
    Cleaning,
    /// The canvas owned by `TypingTabState`.
    Typing,
}

impl CanvasSlot {
    /// Every canvas of the studio, for fan-outs that touch all of them (`for_each_canvas_mut`).
    pub(super) const ALL: [Self; 3] = [Self::Translation, Self::Cleaning, Self::Typing];
}

/// The canvas `tab` draws with, or `None` for a tab without a `CanvasView` (the PS editor has its
/// own camera; the page manager viewer and the document tabs have no canvas). The one owner of
/// the tab -> canvas decision: every per-canvas fan-out in `src/studio` goes through it.
pub(super) fn canvas_slot(tab: AppTab) -> Option<CanvasSlot> {
    match tab {
        AppTab::Translation => Some(CanvasSlot::Translation),
        AppTab::Cleaning => Some(CanvasSlot::Cleaning),
        AppTab::Typing => Some(CanvasSlot::Typing),
        AppTab::PageManager
        | AppTab::PsEditor
        | AppTab::Characters
        | AppTab::Terms
        | AppTab::Notes
        | AppTab::Settings
        | AppTab::Wiki => None,
    }
}

/// Resolves `tab` to its canvas over shared borrows of the three canvas owners.
fn canvas_of<'a>(tab: AppTab, translation: &'a CanvasView, cleaning: &'a CleaningTabState, typing: &'a TypingTabState) -> Option<&'a CanvasView> {
    Some(match canvas_slot(tab)? {
        CanvasSlot::Translation => translation,
        CanvasSlot::Cleaning => cleaning.canvas(),
        CanvasSlot::Typing => typing.canvas(),
    })
}

/// Resolves `slot` to its canvas over mutable borrows of the three canvas owners.
fn slot_canvas_mut<'a>(slot: CanvasSlot, translation: &'a mut CanvasView, cleaning: &'a mut CleaningTabState, typing: &'a mut TypingTabState) -> &'a mut CanvasView {
    match slot {
        CanvasSlot::Translation => translation,
        CanvasSlot::Cleaning => cleaning.canvas_mut(),
        CanvasSlot::Typing => typing.canvas_mut(),
    }
}

/// Calls `f` once for every canvas of the studio, whatever tab is active: in `CanvasSlot::ALL`
/// order, except that `last` (when given) is visited after all the others. A persistence flush
/// passes the ACTIVE tab's canvas as `last`: canvases write whole bubble records with no conflict
/// check, so when two canvases hold edits of one bubble the one written last wins, and the active
/// canvas holds the newest edit (`ViewSync::before_draw` flushes the others when they are left).
pub(super) fn for_each_canvas_mut(translation: &mut CanvasView, cleaning: &mut CleaningTabState, typing: &mut TypingTabState, last: Option<CanvasSlot>, mut f: impl FnMut(CanvasSlot, &mut CanvasView)) {
    for slot in CanvasSlot::ALL.into_iter().filter(|slot| Some(*slot) != last).chain(last) {
        f(slot, slot_canvas_mut(slot, translation, cleaning, typing));
    }
}

/// The view tabs `ViewSync` drives: a borrow of four `Tabs` fields for one call (`Tabs::view_tabs`).
pub(super) struct ViewTabs<'a> {
    /// The Translation tab's canvas (`TranslationTab::canvas_mut`).
    pub(super) translation: &'a mut CanvasView,
    pub(super) cleaning: &'a mut CleaningTabState,
    pub(super) typing: &'a mut TypingTabState,
    pub(super) ps_editor: &'a mut PsEditorTabState,
}

impl ViewTabs<'_> {
    /// The canvas `tab` draws with (see [`canvas_slot`]), mutably.
    pub(super) fn canvas_mut(&mut self, tab: AppTab) -> Option<&mut CanvasView> {
        Some(slot_canvas_mut(canvas_slot(tab)?, self.translation, self.cleaning, self.typing))
    }

    /// Flushes every pending bubble edit of each canvas `active_tab` does not draw
    /// (`CanvasView::flush_all_bubble_edits_now`).
    fn flush_inactive_canvases(&mut self, active_tab: AppTab, project: &ProjectData) {
        let active = canvas_slot(active_tab);
        for slot in CanvasSlot::ALL {
            if Some(slot) != active {
                slot_canvas_mut(slot, self.translation, self.cleaning, self.typing).flush_all_bubble_edits_now(project);
            }
        }
    }

    /// The canvas `tab` draws with (see [`canvas_slot`]).
    fn canvas(&self, tab: AppTab) -> Option<&CanvasView> {
        canvas_of(tab, self.translation, self.cleaning, self.typing)
    }
}

/// Shared view state of the view tabs (canvas tabs + PS editor).
///
/// Invariants: `owner_tab` names the canvas tab whose scroll snapshot `viewport` holds (the PS
/// editor never owns it, it only feeds `viewport.zoom`, `page_idx` and `page_center`);
/// `prev_view_tab` changes only when a view tab finishes a frame, so non-view tabs (Settings,
/// Wiki, …) leave it untouched and a PS -> Settings -> canvas trip is still seen as "from PS".
#[derive(Debug)]
pub(super) struct ViewSync {
    /// The last published canvas viewport (zoom + scroll position).
    viewport: CanvasViewportSnapshot,
    /// The canvas tab `viewport` was taken from, or that last had it applied.
    owner_tab: Option<AppTab>,
    /// Current page shared across view tabs. Lets the PS editor pick up the page the canvas was
    /// showing and hand its page back on the way out.
    page_idx: usize,
    /// Page-local source-pixel center of the shared view, for reproducing the camera position
    /// across tabs that do not share the canvas scroll model. `None` until a view tab publishes it.
    page_center: Option<egui::Vec2>,
    /// The previously active view tab, used to detect transitions into / out of the PS editor
    /// and out of the Typing tab.
    prev_view_tab: Option<AppTab>,
}

impl ViewSync {
    /// Initial state: the Translation canvas owns `initial_viewport`, page 0, no center.
    pub(super) fn new(initial_viewport: CanvasViewportSnapshot) -> Self {
        Self {
            viewport: initial_viewport,
            owner_tab: Some(AppTab::Translation),
            page_idx: 0,
            page_center: None,
            prev_view_tab: Some(AppTab::Translation),
        }
    }

    /// Records an explicit page focus for `tab` (already made active by the caller): the shared
    /// page becomes `page_idx`, the center is reset, `tab`'s canvas (if any) focuses the page at
    /// the shared zoom, and `tab` becomes the owner so the next [`Self::before_draw`] skips its
    /// snapshot re-apply and keeps the focus. `center` is the page center in page-local source
    /// pixels when the page metadata is known; `None` lets the canvas resolve it later.
    pub(super) fn open_page(&mut self, tabs: &mut ViewTabs<'_>, tab: AppTab, page_idx: usize, center: Option<egui::Vec2>) {
        self.page_idx = page_idx;
        self.page_center = None;
        let zoom = self.viewport.zoom;
        if let Some(canvas) = tabs.canvas_mut(tab) {
            canvas.focus_page(page_idx, center, zoom);
        }
        self.owner_tab = Some(tab);
    }

    /// Runs before the active tab draws. Order is load-bearing: first the tab-leave flushes (PS
    /// editor layers, deferred Typing text edits, the pending bubble edits of every canvas the
    /// active tab does not draw), so the destination tab reads the latest layer and bubble state; then the view apply: a canvas tab gets the shared snapshot when it is not its owner,
    /// and is re-centered on the shared page / zoom / center when the previous view tab was the
    /// PS editor; entering the PS editor syncs its page and camera from the shared state.
    pub(super) fn before_draw(&mut self, tabs: &mut ViewTabs<'_>, active_tab: AppTab, project: &ProjectData) {
        let entering_from_ps = self.prev_view_tab == Some(AppTab::PsEditor);
        // Leaving the PS editor: flush its in-memory raster/group edits to disk and bump the shared
        // layer revision, so the destination tab reloads the latest state (e.g. a raster the PS
        // editor just cut into two). PS otherwise only persists on a page-switch, not a tab-switch.
        // Gated on the dirty flag: per-edit changes already enqueued their page write, so an
        // unchanged page skips a redundant snapshot+enqueue on every tab switch.
        if entering_from_ps && active_tab != AppTab::PsEditor {
            tabs.ps_editor.flush_layers_if_dirty(project);
        }
        // Leaving the Text tab: write any DEFERRED text-layer edit. Typing defers edit writes to a
        // focus-loss point, and its own flush points (selection change, page change, idle debounce) all
        // run from its per-frame `draw` — which stops being called the moment the tab is switched away.
        // Without this, the layer that had focus at the switch would keep its edit unwritten until the
        // user came back. Same dirty-gated shape as the PS block above.
        if self.prev_view_tab == Some(AppTab::Typing) && active_tab != AppTab::Typing {
            tabs.typing.flush_text_layers_if_dirty(TypingSaveFlushReason::TabLeave);
        }
        // Every canvas the active tab does not draw: push its pending bubble edits (debounced text,
        // a still-focused bubble) into the bubbles model. A canvas keeps such edits locally and only
        // its own draw would write them, so a left canvas could otherwise hold an OLDER edit of a
        // bubble that the active canvas edits again, and a later flush of both would let the older
        // one win. Runs every frame (a no-op once a left canvas is clean), so every way of switching
        // tabs is covered — tab bar, page-manager open, settings deep link, hotkeys — because the
        // destination tab draws only after this. The left canvas is not drawn while hidden; on return
        // its draw pulls the model as usual (`flush_all_bubble_edits_now` keeps that pull).
        tabs.flush_inactive_canvases(active_tab, project);
        if let Some(canvas) = tabs.canvas_mut(active_tab) {
            // Canvas <-> canvas: reapply the precise scroll snapshot when the owner changed.
            if self.owner_tab != Some(active_tab) {
                canvas.apply_viewport_snapshot(self.viewport);
                self.owner_tab = Some(active_tab);
            }
            // Returning from the PS editor: the scroll snapshot is stale, so re-center the
            // canvas on the page/zoom the PS editor handed back.
            if entering_from_ps {
                canvas.focus_page(self.page_idx, self.page_center, self.viewport.zoom);
            }
        } else if active_tab == AppTab::PsEditor && !entering_from_ps {
            // Entering the PS editor: sync page + clean overlay + camera from the canvas world.
            tabs.ps_editor.sync_view_from_canvas(project, self.page_idx, self.viewport.zoom, self.page_center);
        }
    }

    /// Runs at the end of the frame: a canvas tab publishes its viewport snapshot (and becomes the
    /// owner) plus its current page and page-local center when it has one; the PS editor publishes
    /// its page, zoom and camera center (never the scroll snapshot). A view tab then becomes
    /// `prev_view_tab`; any other tab leaves the state untouched.
    pub(super) fn after_draw(&mut self, tabs: &ViewTabs<'_>, active_tab: AppTab) {
        if let Some(canvas) = tabs.canvas(active_tab) {
            self.viewport = canvas.viewport_snapshot();
            self.owner_tab = Some(active_tab);
            if let Some((page, center)) = canvas.current_page_local_view_center() {
                self.page_idx = page;
                self.page_center = Some(center);
            }
            self.prev_view_tab = Some(active_tab);
        } else if active_tab == AppTab::PsEditor {
            // The PS editor never owns the canvas scroll snapshot, but it does feed the shared
            // page, zoom, and camera center back so the canvas can follow it on the way out.
            if let Some(page) = tabs.ps_editor.current_page() {
                self.page_idx = page;
            }
            let (zoom, center) = tabs.ps_editor.camera();
            self.viewport.zoom = zoom;
            self.page_center = Some(center);
            self.prev_view_tab = Some(active_tab);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CanvasSlot, ViewSync, ViewTabs, canvas_slot, for_each_canvas_mut};
    use crate::canvas::CanvasView;
    use crate::project::ProjectData;
    use crate::tabs::AppTab;
    use crate::tabs::cleaning::CleaningTabState;
    use crate::tabs::ps_editor::PsEditorTabState;
    use crate::tabs::typing::TypingTabState;
    use crate::project::SessionKind;
    use std::path::PathBuf;
    use std::sync::Arc;
    use eframe::egui;

    /// A project with no pages and empty paths: nothing in the view-sync path reaches the disk
    /// with default (clean, unbound) tab states, and the PS editor ignores an unknown page.
    fn empty_project() -> ProjectData {
        use crate::project::{CanvasSettings, ProjectPaths};
        let empty = PathBuf::new;
        ProjectData {
            project_dir: empty(),
            image_dir: empty(),
            pages: Vec::new(),
            bubbles: Arc::new(Vec::new()),
            paths: ProjectPaths {
                project_dir: empty(),
                title_dir: empty(),
                notes_file: empty(),
                char_favorites_file: empty(),
                color_presets_file: empty(),
                bubbles_file: empty(),
                src_dir: empty(),
                clean_layers_dir: empty(),
                cleaned_dir: empty(),
                alt_vers_dir: empty(),
                saved_dir: empty(),
                image_bubbles_dir: empty(),
                text_images_dir: empty(),
                layers_dir: empty(),
                text_detection_dir: empty(),
                characters_dir: empty(),
                terms_file: empty(),
                settings_file: empty(),
                unsaved_dir: empty(),
                unsaved_bubbles_file: empty(),
                unsaved_clean_layers_dir: empty(),
                unsaved_image_bubbles_dir: empty(),
                unsaved_text_images_dir: empty(),
                unsaved_layers_dir: empty(),
            },
            comic_type: None,
            canvas_settings: CanvasSettings::default(),
            settings_data: serde_json::Value::Null,
            session: SessionKind::Project,
        }
    }

    /// Exactly the three canvas tabs map to a canvas, each to its own.
    #[test]
    fn canvas_slot_maps_only_canvas_tabs() {
        for tab in AppTab::ALL {
            let expected = match tab {
                AppTab::Translation => Some(CanvasSlot::Translation),
                AppTab::Cleaning => Some(CanvasSlot::Cleaning),
                AppTab::Typing => Some(CanvasSlot::Typing),
                AppTab::PageManager
                | AppTab::PsEditor
                | AppTab::Characters
                | AppTab::Terms
                | AppTab::Notes
                | AppTab::Settings
                | AppTab::Wiki => None,
            };
            assert_eq!(canvas_slot(tab), expected, "{tab:?}");
        }
    }

    /// The all-canvas fan-out visits each canvas exactly once, in `CanvasSlot::ALL` order with the
    /// `last` slot moved to the end, and each slot resolves to the canvas the tab mapping gives.
    #[test]
    fn for_each_canvas_visits_every_canvas_once_with_the_active_one_last() {
        use CanvasSlot::{Cleaning, Translation, Typing};
        let mut translation = CanvasView::default();
        let mut cleaning = CleaningTabState::default();
        let mut typing = TypingTabState::default();
        let mut pointers = Vec::new();
        for_each_canvas_mut(&mut translation, &mut cleaning, &mut typing, None, |_, canvas| pointers.push(std::ptr::from_mut(canvas)));
        assert_eq!(pointers, vec![std::ptr::from_mut(&mut translation), std::ptr::from_mut(cleaning.canvas_mut()), std::ptr::from_mut(typing.canvas_mut())]);
        for (last, expected) in [(None, [Translation, Cleaning, Typing]), (Some(Translation), [Cleaning, Typing, Translation]), (Some(Cleaning), [Translation, Typing, Cleaning]), (Some(Typing), [Translation, Cleaning, Typing])] {
            let mut visited = Vec::new();
            for_each_canvas_mut(&mut translation, &mut cleaning, &mut typing, last, |slot, _| visited.push(slot));
            assert_eq!(visited, expected.to_vec(), "last = {last:?}");
        }
    }

    /// Translation -> Cleaning -> PS editor -> Settings -> Typing with default (never laid out)
    /// canvases: the zoom follows the shared viewport canvas to canvas, the PS editor's camera
    /// zoom is handed back to the canvas entered after it (through a non-view tab), and the
    /// owner / previous-view bookkeeping matches each step.
    #[test]
    fn view_sync_round_trip_through_ps_editor() {
        let mut translation = CanvasView::default();
        let mut cleaning = CleaningTabState::default();
        let mut typing = TypingTabState::default();
        let mut ps_editor = PsEditorTabState::default();
        let project = empty_project();
        let mut tabs = ViewTabs { translation: &mut translation, cleaning: &mut cleaning, typing: &mut typing, ps_editor: &mut ps_editor };
        let mut sync = ViewSync::new(tabs.translation.viewport_snapshot());

        // Translation draws at zoom 2.0 and publishes it.
        sync.before_draw(&mut tabs, AppTab::Translation, &project);
        tabs.translation.focus_page(0, None, 2.0);
        sync.after_draw(&tabs, AppTab::Translation);
        assert_eq!(sync.owner_tab, Some(AppTab::Translation));
        assert_eq!(sync.viewport.zoom, tabs.translation.viewport_snapshot().zoom);
        let shared_zoom = sync.viewport.zoom;

        // Cleaning is not the owner: the snapshot zoom is applied to its canvas.
        sync.before_draw(&mut tabs, AppTab::Cleaning, &project);
        assert_eq!(sync.owner_tab, Some(AppTab::Cleaning));
        assert_eq!(tabs.cleaning.canvas().viewport_snapshot().zoom, shared_zoom);
        sync.after_draw(&tabs, AppTab::Cleaning);
        assert_eq!(sync.prev_view_tab, Some(AppTab::Cleaning));

        // PS editor: publishes its camera zoom and center, never the owner.
        sync.before_draw(&mut tabs, AppTab::PsEditor, &project);
        sync.after_draw(&tabs, AppTab::PsEditor);
        let (ps_zoom, ps_center) = tabs.ps_editor.camera();
        assert_eq!(sync.viewport.zoom, ps_zoom);
        assert_eq!(sync.page_center, Some(ps_center));
        assert_eq!(sync.owner_tab, Some(AppTab::Cleaning));
        assert_eq!(sync.prev_view_tab, Some(AppTab::PsEditor));

        // A non-view tab keeps the state, so Typing afterwards still counts as "from PS".
        sync.before_draw(&mut tabs, AppTab::Settings, &project);
        sync.after_draw(&tabs, AppTab::Settings);
        assert_eq!(sync.prev_view_tab, Some(AppTab::PsEditor));

        sync.before_draw(&mut tabs, AppTab::Typing, &project);
        assert_eq!(sync.owner_tab, Some(AppTab::Typing));
        let mut expected = CanvasView::default();
        expected.focus_page(0, Some(ps_center), ps_zoom);
        assert_eq!(tabs.typing.canvas().viewport_snapshot().zoom, expected.viewport_snapshot().zoom);
        sync.after_draw(&tabs, AppTab::Typing);
        assert_eq!(sync.prev_view_tab, Some(AppTab::Typing));
    }

    /// Canvas hooks that add nothing: the canvas draws its own content only.
    struct NoHooks;
    impl crate::canvas::CanvasHooks for NoHooks {}

    /// One headless frame of `canvas` (zero pages, no input). `CanvasView::draw` pulls the bubbles
    /// model at its start, exactly as a frame of the canvas's tab does.
    fn draw_canvas_headless(ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        use crate::canvas::{CanvasDrawParams, CanvasUiStatus, SourceTextureUploadBudget};
        let page_infos = std::collections::HashMap::new();
        let mut textures = std::collections::HashMap::new();
        let mut panel_dock = crate::widgets::panel_dock::PanelDockState::new();
        let status = CanvasUiStatus { loaded_pages: 0, total_pages: 0, load_errors_count: 0 };
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut source_upload_budget = SourceTextureUploadBudget::source_page_reupload_default();
            canvas.draw(CanvasDrawParams { ctx, ui, project, page_infos: &page_infos, texture_cache: &mut textures, status, source_upload_budget: &mut source_upload_budget, hooks: &mut NoHooks, panel_dock: &mut panel_dock });
        })
        .drop_without_applying_deltas();
    }

    /// The text of bubble 1 as `canvas` currently holds it (its runtime copy).
    fn canvas_text(canvas: &CanvasView, project: &ProjectData) -> String {
        canvas.hook_bubbles_snapshot(project).into_iter().find(|bubble| bubble.id == 1).map(|bubble| bubble.text).unwrap_or_default()
    }

    /// Two canvases edit one bubble, driven through the production frame order (`before_draw`,
    /// the active canvas's draw, `after_draw`): an OLDER edit is left pending in the Cleaning
    /// canvas, the user switches to Translation, and `before_draw`'s leave flush writes it before
    /// the Translation canvas pulls the model; the NEWER edit is made there. The save fan-out
    /// (`for_each_canvas_mut` with the active canvas last, as `SessionParts::flush_canvas_bubble_edits`)
    /// leaves the newest text in the model, the active canvas shows it on its next sync, and so
    /// does the left canvas when its tab is entered again.
    #[test]
    fn leave_flush_then_save_fan_out_keeps_the_newest_edit() {
        use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
        use crate::project::Bubble;
        use std::sync::Mutex;

        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!("ms_view_sync_leave_flush_{}_{nanos}", std::process::id()));
        let bubble = Bubble { id: 1, img_idx: 0, img_u: 0.3, img_v: 0.3, side: Some("left".to_string()), bubble_class: Some("text".to_string()), bubble_type: Some("aside".to_string()), text: String::new(), original_text: String::new(), extra: serde_json::Map::new() };
        let model = Arc::new(Mutex::new(BubblesModel::new(vec![bubble], root.join("bubbles.json"), root.join("unsaved_bubbles.json"), SharedCanvasSettings::default(), None)));
        let model_text = || model.lock().expect("lock model").with_bubble(1, |bubble| bubble.text.clone()).expect("bubble 1 exists");
        let project = empty_project();
        let ctx = egui::Context::default();

        let mut translation = CanvasView::default();
        let mut cleaning = CleaningTabState::default();
        let mut typing = TypingTabState::default();
        let mut ps_editor = PsEditorTabState::default();
        translation.set_bubbles_model(Arc::clone(&model));
        cleaning.set_bubbles_model(Arc::clone(&model));
        {
            let mut tabs = ViewTabs { translation: &mut translation, cleaning: &mut cleaning, typing: &mut typing, ps_editor: &mut ps_editor };
            let mut sync = ViewSync::new(tabs.translation.viewport_snapshot());

            // A Translation frame, then a Cleaning frame in which the user types the OLDER edit
            // (debounced, still pending when the tab is left).
            sync.before_draw(&mut tabs, AppTab::Translation, &project);
            draw_canvas_headless(&ctx, tabs.translation, &project);
            sync.after_draw(&tabs, AppTab::Translation);
            sync.before_draw(&mut tabs, AppTab::Cleaning, &project);
            draw_canvas_headless(&ctx, tabs.cleaning.canvas_mut(), &project);
            assert!(tabs.cleaning.canvas_mut().set_bubble_texts_from_panel(1, Some("older".to_string()), None, 0.0, false));
            sync.after_draw(&tabs, AppTab::Cleaning);
            assert_eq!(model_text(), "", "the cleaning edit is still pending in its canvas");

            // Switch to Translation: the leave flush writes the older edit BEFORE Translation draws,
            // so its draw pulls it; then the user types the NEWER edit there.
            sync.before_draw(&mut tabs, AppTab::Translation, &project);
            assert_eq!(model_text(), "older", "before_draw flushed the canvas being left");
            draw_canvas_headless(&ctx, tabs.translation, &project);
            assert_eq!(canvas_text(tabs.translation, &project), "older", "the active canvas pulled the left canvas's edit");
            assert!(tabs.translation.set_bubble_texts_from_panel(1, Some("newer".to_string()), None, 0.0, false));
            sync.after_draw(&tabs, AppTab::Translation);

            // A persistence point: every canvas flushed, the active one last.
            for_each_canvas_mut(tabs.translation, tabs.cleaning, tabs.typing, canvas_slot(AppTab::Translation), |_, canvas| canvas.flush_all_bubble_edits_now(&project));
            assert_eq!(model_text(), "newer", "the newest edit wins the save fan-out");

            // The active canvas's next frame syncs and still shows the newest text.
            sync.before_draw(&mut tabs, AppTab::Translation, &project);
            draw_canvas_headless(&ctx, tabs.translation, &project);
            assert_eq!(canvas_text(tabs.translation, &project), "newer");
            sync.after_draw(&tabs, AppTab::Translation);
            assert_eq!(model_text(), "newer");

            // Back on Cleaning: its canvas pulls the newest text too.
            sync.before_draw(&mut tabs, AppTab::Cleaning, &project);
            draw_canvas_headless(&ctx, tabs.cleaning.canvas_mut(), &project);
            assert_eq!(canvas_text(tabs.cleaning.canvas(), &project), "newer");
            assert_eq!(model_text(), "newer");
        }
        drop((translation, cleaning, typing, ps_editor));
        assert!(model.lock().expect("lock model").shutdown_saver().is_ok(), "bubbles saver shut down");
        drop(model);
        if root.exists() {
            assert!(std::fs::remove_dir_all(&root).is_ok(), "scratch dir {} not removed", root.display());
        }
    }

    /// An explicit page open makes the destination the owner, so the next `before_draw` keeps
    /// the focus instead of re-applying the shared snapshot.
    #[test]
    fn open_page_takes_ownership() {
        let mut translation = CanvasView::default();
        let mut cleaning = CleaningTabState::default();
        let mut typing = TypingTabState::default();
        let mut ps_editor = PsEditorTabState::default();
        let mut tabs = ViewTabs { translation: &mut translation, cleaning: &mut cleaning, typing: &mut typing, ps_editor: &mut ps_editor };
        let mut sync = ViewSync::new(tabs.translation.viewport_snapshot());
        sync.open_page(&mut tabs, AppTab::Cleaning, 3, None);
        assert_eq!(sync.owner_tab, Some(AppTab::Cleaning));
        assert_eq!(sync.page_idx, 3);
        assert_eq!(sync.page_center, None);
        // A tab without a canvas still records the page and the owner.
        sync.open_page(&mut tabs, AppTab::PsEditor, 5, None);
        assert_eq!(sync.owner_tab, Some(AppTab::PsEditor));
        assert_eq!(sync.page_idx, 5);
    }
}
