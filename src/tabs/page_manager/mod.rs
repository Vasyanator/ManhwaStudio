/*
File: tabs/page_manager/mod.rs

Purpose:
"Page manager" studio tab: a card grid of the chapter's pages with selection,
badges (clean overlay / bubble count / layer count), and structural page
operations (insert / create blank / move / delete) requested from the app
through `PageManagerAction`.

Key structures:
- PageManagerTabState: tab state — shared-model handles, selection, badge
  caches, the thumbnail/scan worker, and the open dialog.
- PageManagerAction: what the tab asks the app to do (run a `PageOpKind`,
  or switch to another tab focused on a page).

Key functions:
- PageManagerTabState::draw(): per-frame entry point (toolbar / grid / status /
  dialogs), returns the requested actions.
- notify_pages_changed(): cache invalidation hook the app calls after a
  structural operation or project reload.

Notes:
This tab is NOT a `CanvasView`. It never executes structural operations itself:
it only emits `PageManagerAction::RequestOp`, and the app quiesces writers,
executes the op, reloads the project, and calls `notify_pages_changed`.
All disk work (thumbnail decode, `layers.json` scans) runs on the worker in
`thumbs.rs`; shared-model locks are short and revision-gated per frame.
*/

mod dialogs;
mod grid;
mod thumbs;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::app::PageImageInfo;
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::layer_model::layer_doc::LayerDoc;
use crate::page_ops::PageOpKind;
use crate::project::ProjectData;
use crate::widgets::WheelSpinBox;

use dialogs::PageManagerDialog;
use thumbs::ThumbRuntime;

/// File name of the layer manifest inside a layers directory. Persistence
/// identifier (matches `models/layer_model/persist.rs::MANIFEST_FILE`).
const LAYERS_MANIFEST_FILE: &str = "layers.json";

/// A request the page-manager tab emits for the app root to execute.
#[derive(Debug, Clone)]
pub enum PageManagerAction {
    /// Ask the app to run a structural page operation (quiesce + execute +
    /// project reload).
    RequestOp(PageOpKind),
    /// Switch to `tab` and focus page `page_idx` there.
    OpenPageIn {
        tab: crate::tabs::AppTab,
        page_idx: usize,
    },
}

/// State of the "Page manager" tab. Construct with `Default`, wire the shared
/// models through the `set_*` methods (mirroring the other tabs' wiring in
/// `MangaApp::new`), then call [`Self::draw`] every frame the tab is active.
pub struct PageManagerTabState {
    bubbles_model: Option<Arc<Mutex<BubblesModel>>>,
    overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    layer_doc: Option<Arc<Mutex<LayerDoc>>>,

    /// `src/` dir of the project the caches were built for; a change resets the tab.
    loaded_src_dir: Option<PathBuf>,
    /// Bumped by [`Self::notify_pages_changed`]; thumbnails re-validate their
    /// (path, mtime) cache key against it.
    generation: u64,

    /// Selected CURRENT page indices (order of `ProjectData::pages`).
    selection: BTreeSet<usize>,
    /// Anchor of Shift range selection (last plain/Ctrl-clicked card).
    selection_anchor: Option<usize>,
    /// 1-based target for the "move to position" toolbar control.
    move_to_position: usize,

    /// Thumbnail decode + manifest scan worker and the LRU thumbnail cache.
    thumbs: ThumbRuntime,

    // --- badge caches, recomputed only when the source revision changes ---
    /// `BubblesModel::revision` the counts were computed at.
    bubbles_revision_seen: Option<u64>,
    /// Bubbles per page index.
    bubble_counts: HashMap<usize, usize>,
    /// Total bubble count for the status line.
    bubble_total: usize,
    /// `CleanOverlaysModel::revision` the flags were computed at.
    overlays_revision_seen: Option<u64>,
    /// Whether page `idx` has any clean-overlay content.
    clean_present: Vec<bool>,
    /// `LayerDoc::version` the resident counts were computed at.
    layer_doc_version_seen: Option<u64>,
    /// Live layer counts of pages resident in the shared `LayerDoc` (fresher
    /// than the manifest, overrides it per page).
    resident_layer_counts: HashMap<usize, usize>,
    /// Layer counts scanned from `layers.json` on the worker.
    manifest_layer_counts: HashMap<usize, usize>,
    /// Current manifest-scan epoch; results from older epochs are dropped.
    scan_epoch: u64,
    /// Epoch a scan has already been submitted for.
    scan_requested_epoch: Option<u64>,

    /// The currently open modal dialog, if any.
    dialog: Option<PageManagerDialog>,
    /// Last user-facing error (e.g. the file-picker worker died).
    error_message: Option<String>,
}

impl Default for PageManagerTabState {
    fn default() -> Self {
        Self {
            bubbles_model: None,
            overlays_model: None,
            layer_doc: None,
            loaded_src_dir: None,
            generation: 0,
            selection: BTreeSet::new(),
            selection_anchor: None,
            move_to_position: 1,
            thumbs: ThumbRuntime::default(),
            bubbles_revision_seen: None,
            bubble_counts: HashMap::new(),
            bubble_total: 0,
            overlays_revision_seen: None,
            clean_present: Vec::new(),
            layer_doc_version_seen: None,
            resident_layer_counts: HashMap::new(),
            manifest_layer_counts: HashMap::new(),
            scan_epoch: 0,
            scan_requested_epoch: None,
            dialog: None,
            error_message: None,
        }
    }
}

impl PageManagerTabState {
    /// Wires the shared bubbles model (same instance the canvas tabs hold).
    pub fn set_bubbles_model(&mut self, model: Arc<Mutex<BubblesModel>>) {
        self.bubbles_model = Some(model);
        self.bubbles_revision_seen = None;
    }

    /// Wires the shared clean-overlays model.
    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.overlays_model = Some(model);
        self.overlays_revision_seen = None;
    }

    /// Wires the app-owned shared unified layer document (same wiring shape as
    /// the typing / PS-editor tabs).
    pub fn set_layer_doc(&mut self, doc: Arc<Mutex<LayerDoc>>) {
        self.layer_doc = Some(doc);
        self.layer_doc_version_seen = None;
    }

    /// Invalidates thumbnails and scans. The app calls this after a structural
    /// page operation or a project reload: page indices may have shifted, so the
    /// selection and any open dialog are dropped and every cache re-validates.
    pub fn notify_pages_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        self.scan_requested_epoch = None;
        self.selection.clear();
        self.selection_anchor = None;
        self.dialog = None;
        self.bubbles_revision_seen = None;
        self.overlays_revision_seen = None;
        self.layer_doc_version_seen = None;
        self.resident_layer_counts.clear();
        self.manifest_layer_counts.clear();
    }

    /// Per-frame entry point. Renders the toolbar, the card grid, the status
    /// line, and any open dialog, and returns the actions the app must execute.
    /// `op_in_progress` disables every structural button while an operation or
    /// save is running.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        project: &ProjectData,
        page_infos: &HashMap<usize, PageImageInfo>,
        op_in_progress: bool,
    ) -> Vec<PageManagerAction> {
        let mut actions = Vec::new();
        let page_count = project.pages.len();

        self.ensure_project(project);
        self.absorb_worker_events(ctx);
        self.refresh_badges(page_count);
        self.request_layers_scan_if_needed(project);
        self.clamp_selection(page_count);

        egui::Panel::top("page_manager_toolbar").show(ui, |ui| {
            self.draw_toolbar(ui, page_count, op_in_progress, &mut actions);
        });
        egui::Panel::bottom("page_manager_status").show(ui, |ui| {
            self.draw_status(ui, page_count);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.draw_grid(ui, project, page_infos, op_in_progress, &mut actions);
        });

        // The create dialog seeds its default size from a neighbouring page:
        // authoritative geometry first, thumbnail-probed size second. Sizes are
        // snapshotted into an owned map so the lookup closure does not keep
        // `self` borrowed while `draw_dialogs` needs it mutably; the O(pages)
        // walk only runs while the create dialog is open.
        let mut known_sizes: HashMap<usize, (u32, u32)> = HashMap::new();
        if matches!(self.dialog, Some(PageManagerDialog::Create(_))) {
            for (idx, page) in project.pages.iter().enumerate() {
                let size = page_infos
                    .get(&idx)
                    .map(|info| (info.width_px, info.height_px))
                    .filter(|(w, h)| *w > 0 && *h > 0)
                    .or_else(|| {
                        self.thumbs
                            .cache
                            .peek(&page.path)
                            .and_then(|entry| entry.full_size)
                    });
                if let Some(size) = size {
                    known_sizes.insert(idx, size);
                }
            }
        }
        let size_of = move |idx: usize| known_sizes.get(&idx).copied();
        self.draw_dialogs(ctx, page_count, &size_of, op_in_progress, &mut actions);

        // Keep frames coming while background work is pending so its results land.
        if self.thumbs.has_in_flight()
            || self
                .dialog
                .as_ref()
                .is_some_and(PageManagerDialog::picker_active)
        {
            ctx.request_repaint();
        }
        actions
    }

    /// Resets the tab when a different project (chapter) is shown.
    fn ensure_project(&mut self, project: &ProjectData) {
        let src_dir = project.paths.src_dir.clone();
        let changed = self
            .loaded_src_dir
            .as_ref()
            .map(|existing| existing != &src_dir)
            .unwrap_or(true);
        if !changed {
            return;
        }
        self.loaded_src_dir = Some(src_dir);
        self.thumbs.reset();
        self.error_message = None;
        self.move_to_position = 1;
        self.notify_pages_changed();
    }

    /// Drains thumbnail/scan worker events; applies layer scans of the current epoch.
    fn absorb_worker_events(&mut self, ctx: &egui::Context) {
        for (epoch, counts) in self.thumbs.poll(ctx) {
            if epoch == self.scan_epoch {
                self.manifest_layer_counts = counts;
            }
        }
    }

    /// Recomputes the badge caches whose source revision changed. Every shared
    /// model lock here is short: snapshot/flags are copied out and all counting
    /// happens after the lock is released.
    fn refresh_badges(&mut self, page_count: usize) {
        if let Some(model) = self.bubbles_model.as_ref()
            && let Ok(guard) = model.lock()
        {
            let revision = guard.revision();
            if self.bubbles_revision_seen != Some(revision) {
                let snapshot = guard.snapshot_shared();
                drop(guard);
                self.bubbles_revision_seen = Some(revision);
                self.bubble_counts.clear();
                self.bubble_total = snapshot.len();
                for bubble in snapshot.iter() {
                    *self.bubble_counts.entry(bubble.img_idx).or_insert(0) += 1;
                }
            }
        }
        if let Some(model) = self.overlays_model.as_ref()
            && let Ok(guard) = model.lock()
        {
            let revision = guard.revision();
            if self.overlays_revision_seen != Some(revision)
                || self.clean_present.len() != page_count
            {
                self.overlays_revision_seen = Some(revision);
                self.clean_present = (0..page_count)
                    .map(|idx| !guard.is_overlay_virtual_absent(idx))
                    .collect();
            }
        }
        if let Some(doc) = self.layer_doc.as_ref()
            && let Ok(guard) = doc.lock()
        {
            let version = guard.version();
            if self.layer_doc_version_seen != Some(version) {
                self.layer_doc_version_seen = Some(version);
                self.resident_layer_counts = guard
                    .resident_pages()
                    .into_iter()
                    .filter_map(|idx| Some((idx, guard.page(idx)?.nodes.len())))
                    .collect();
            }
        }
    }

    /// Submits a `layers.json` scan for the current epoch if none is pending.
    fn request_layers_scan_if_needed(&mut self, project: &ProjectData) {
        if self.scan_requested_epoch == Some(self.scan_epoch) {
            return;
        }
        self.scan_requested_epoch = Some(self.scan_epoch);
        self.thumbs.request_layers_scan(
            self.scan_epoch,
            project.paths.layers_dir.join(LAYERS_MANIFEST_FILE),
            project.paths.unsaved_layers_dir.join(LAYERS_MANIFEST_FILE),
        );
    }

    /// Drops selected indices that no longer exist (e.g. right after a reload).
    fn clamp_selection(&mut self, page_count: usize) {
        self.selection.retain(|idx| *idx < page_count);
        if self
            .selection_anchor
            .is_some_and(|anchor| anchor >= page_count)
        {
            self.selection_anchor = None;
        }
        self.move_to_position = self.move_to_position.clamp(1, page_count.max(1));
    }

    /// The selected page index when EXACTLY one page is selected (move
    /// operations are defined for a single page).
    fn single_selection(&self) -> Option<usize> {
        if self.selection.len() == 1 {
            self.selection.first().copied()
        } else {
            None
        }
    }

    /// Layer count of page `idx`: live in-memory count for pages resident in the
    /// shared `LayerDoc`, manifest-scanned count otherwise.
    fn effective_layer_count(&self, idx: usize) -> usize {
        self.resident_layer_counts
            .get(&idx)
            .or_else(|| self.manifest_layer_counts.get(&idx))
            .copied()
            .unwrap_or(0)
    }

    /// Draws the structural-operations toolbar.
    fn draw_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        page_count: usize,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) {
        if let Some(error) = &self.error_message {
            ui.colored_label(ui.visuals().warn_fg_color, error);
        }
        ui.horizontal_wrapped(|ui| {
            let insert_response = ui.add_enabled(
                !op_in_progress,
                egui::Button::new(t!("page_manager.toolbar.insert_pages_button")),
            );
            if insert_response.clicked() {
                self.error_message = None;
                self.dialog = Some(PageManagerDialog::insert(!self.selection.is_empty()));
            }
            if ui
                .add_enabled(
                    !op_in_progress,
                    egui::Button::new(t!("page_manager.toolbar.create_page_button")),
                )
                .clicked()
            {
                self.error_message = None;
                self.dialog = Some(PageManagerDialog::create(!self.selection.is_empty()));
            }
            if ui
                .add_enabled(
                    !op_in_progress && !self.selection.is_empty(),
                    egui::Button::new(t!("page_manager.toolbar.delete_button")),
                )
                .on_disabled_hover_text(if op_in_progress {
                    t!("page_manager.toolbar.op_in_progress_hint")
                } else {
                    t!("page_manager.toolbar.requires_selection_hint")
                })
                .clicked()
            {
                self.dialog = Some(PageManagerDialog::delete(
                    self.selection.iter().copied().collect(),
                ));
            }

            ui.separator();

            let single = self.single_selection();
            let single_hint = || {
                if op_in_progress {
                    t!("page_manager.toolbar.op_in_progress_hint")
                } else {
                    t!("page_manager.toolbar.single_selection_hint")
                }
            };
            if ui
                .add_enabled(
                    !op_in_progress && single.is_some_and(|idx| idx > 0),
                    egui::Button::new(t!("page_manager.toolbar.move_up_button")),
                )
                .on_disabled_hover_text(single_hint())
                .clicked()
                && let Some(from) = single
            {
                // Move semantics (see PageOpKind::Move): `to` indexes the NEW
                // order, so one step up is exactly `from - 1`.
                actions.push(PageManagerAction::RequestOp(PageOpKind::Move {
                    from,
                    to: from.saturating_sub(1),
                }));
            }
            if ui
                .add_enabled(
                    !op_in_progress && single.is_some_and(|idx| idx + 1 < page_count),
                    egui::Button::new(t!("page_manager.toolbar.move_down_button")),
                )
                .on_disabled_hover_text(single_hint())
                .clicked()
                && let Some(from) = single
            {
                actions.push(PageManagerAction::RequestOp(PageOpKind::Move {
                    from,
                    to: from + 1,
                }));
            }

            ui.separator();

            ui.label(t!("page_manager.toolbar.move_to_label"));
            ui.add_enabled(
                !op_in_progress && single.is_some() && page_count > 0,
                WheelSpinBox::new(&mut self.move_to_position).range(1..=page_count.max(1)),
            );
            if ui
                .add_enabled(
                    !op_in_progress
                        && single.is_some_and(|idx| self.move_to_position != idx + 1),
                    egui::Button::new(t!("page_manager.toolbar.move_to_button")),
                )
                .on_disabled_hover_text(single_hint())
                .clicked()
                && let Some(from) = single
            {
                // 1-based UI position P -> new-order index P-1.
                actions.push(PageManagerAction::RequestOp(PageOpKind::Move {
                    from,
                    to: self.move_to_position.saturating_sub(1),
                }));
            }
        });
    }

    /// Draws the status line: total pages / pages with clean / total bubbles.
    fn draw_status(&self, ui: &mut egui::Ui, page_count: usize) {
        let cleaned = self.clean_present.iter().filter(|present| **present).count();
        ui.label(tf!(
            "page_manager.status.summary",
            pages = page_count,
            cleaned = cleaned,
            bubbles = self.bubble_total
        ));
    }
}
