/*
File: src/studio/session/mod.rs

Purpose:
The studio session strategy: `SessionMode`, ONE enum chosen once in `StudioApp::new` from the
opened `ProjectData`, with a variant per session kind — a project chapter (`ProjectSession`) or an
image set (`ImageSetSession`). Every place where `StudioApp` behaves differently per kind calls one
dispatch method here; nothing else in `src/studio` asks which kind is open.

Key structures:
- `SessionMode`: `Project(ProjectSession)` | `ImageSet(Box<ImageSetSession>)`.
- `SessionParts`: the `StudioApp` parts a session flow borrows for one call (project, staging
  writers, close intent, the tab states it flushes or notifies).

Key functions:
- `SessionMode::from_project`: the one constructor (and the one session-kind decision in `src/studio`).
- Dispatch: `has_unsaved_changes`, `tab_visible`, `hotkey_specs`, `capabilities` (the tabs' `SessionCapabilities`), `tick`, `draw_top_bar`,
  `draw_exit_dialog`, `draw_dialogs`, `on_save_hotkey`, `start_page_op`, `before_shutdown`, plus
  the project-state queries (`busy_jobs`, `exit_cleanup_action`, `take_reload_request`,
  `note_unsaved`), which answer "nothing" for an image set; `take_title` (image set only); `os_close_policy` (a second OS close
  over the exit dialog: a project exits, an image set keeps the dialog).

Notes:
The image-set arm never writes project staging: every close of it is the DISCARD quiesce
(`close_image_set_discarding`, and `before_shutdown`), because its staging is a scratch the
window host deletes after the app was dropped. Project flows and their order live in `project/`;
the image-set controller in `image_set/`. Neither reaches `StudioApp`: they get `SessionParts`.
*/

pub(crate) mod image_set;
mod project;

use self::image_set::{ExitChoice, ImageSetSession, SaveOutcome, SaveParts};
use self::project::ProjectSession;
use super::PendingCloseAction;
use super::busy::PersistenceJobs;
use super::close_flow::{CloseFlow, OsClosePolicy};
use super::staging_writers::StagingWriters;
use super::view_sync::{canvas_slot, for_each_canvas_mut};
use crate::canvas::CanvasView;
use crate::input_manager_v2::HotkeySpecV2;
use crate::models::SessionCapabilities;
use crate::page_ops::PageOpKind;
use crate::project::ProjectData;
use crate::runtime_log;
use crate::tabs::AppTab;
use crate::tabs::cleaning::CleaningTabState;
use crate::tabs::page_manager::PageManagerTabState;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::TypingTabState;
use eframe::egui;
use serde_json::Value;

/// The `StudioApp` parts a session flow borrows for one call (disjoint fields of the app, built by
/// `StudioApp::session_parts`). A flow reaches the app ONLY through this struct.
pub(in crate::studio) struct SessionParts<'a> {
    /// The open chapter (or the image set's scratch chapter).
    pub(in crate::studio) project: &'a ProjectData,
    /// The staging writers, their autosave gate, the DISCARD latch and the shared models.
    pub(in crate::studio) writers: &'a mut StagingWriters,
    /// The close intent: exit dialog, close request, received-images question.
    pub(in crate::studio) close: &'a mut CloseFlow,
    /// Text flushes, the layer preload, the flatten-to-file API, the export flag.
    pub(in crate::studio) typing: &'a mut TypingTabState,
    /// Raster-layer flushes before a save or a page op.
    pub(in crate::studio) ps_editor: &'a mut PsEditorTabState,
    /// The translation canvas; with the cleaning and typing tabs' canvases, flushed into the
    /// bubbles model at every persistence point (`flush_canvas_bubble_edits`).
    pub(in crate::studio) translation_canvas: &'a mut CanvasView,
    /// The tab the user is on: its canvas (if any) is flushed LAST by `flush_canvas_bubble_edits`.
    pub(in crate::studio) active_tab: AppTab,
    /// Told to rescan its clean-folder status after a merge; owns the cleaning canvas.
    pub(in crate::studio) cleaning: &'a mut CleaningTabState,
    /// Its clean-op flag gates saves and page ops; told to rescan its clean cards after a merge.
    pub(in crate::studio) page_manager: &'a mut PageManagerTabState,
}

impl SessionParts<'_> {
    /// Pushes every pending bubble edit of the three canvases (translation, cleaning, typing) into
    /// the shared `BubblesModel` (`CanvasView::flush_all_bubble_edits_now`: debounced text and
    /// focused bubbles included), the ACTIVE tab's canvas last. Normally only the active canvas
    /// holds edits (`ViewSync::before_draw` flushes a canvas once its tab is left); the others are
    /// a safety net, written first so that if two canvases ever hold edits of one bubble the
    /// active canvas's — the newest — wins (records are written whole, without a conflict check).
    /// Cheap, no file I/O: the model queues a snapshot for its saver, so a later bubbles-saver
    /// barrier covers it. Called at the start of save-to-project, of a page op and of the
    /// non-discard teardown; never on the DISCARD path.
    pub(in crate::studio) fn flush_canvas_bubble_edits(&mut self) {
        let project = self.project;
        for_each_canvas_mut(self.translation_canvas, self.cleaning, self.typing, canvas_slot(self.active_tab), |_, canvas| canvas.flush_all_bubble_edits_now(project));
    }
}

/// The session strategy of one `StudioApp` (see the file header). Built once; the kind never changes
/// for the life of the app (a reload builds a new app).
pub(in crate::studio) enum SessionMode {
    /// A chapter of a project: staging merged by save-to-project, structural page ops, DISCARD.
    Project(ProjectSession),
    /// One image or an ordered image set: saved to its files, never merged, every close discards.
    /// Boxed: the controller is several hundred bytes, the project state a few dozen.
    ImageSet(Box<ImageSetSession>),
}

impl SessionMode {
    /// The session of `project`: an image set when `project.session()` is `SessionKind::SingleImage`
    /// (decided by `ImageSetSession::from_project`, which reads the kind; never on wasm), otherwise
    /// a project session. `user_settings` is the startup `user_config` root (JPEG quality default);
    /// `has_staging` whether the chapter's staging directory existed at open
    /// (`StudioBootstrap::has_staging`, the project session's initial unsaved flag). No I/O.
    #[must_use]
    pub(in crate::studio) fn from_project(project: &ProjectData, user_settings: &Value, has_staging: bool) -> Self {
        match ImageSetSession::from_project(project, user_settings) {
            Some(image_set) => Self::ImageSet(Box::new(image_set)),
            None => Self::Project(ProjectSession::new(has_staging)),
        }
    }

    /// Whether this is an image-set session.
    #[must_use]
    pub(in crate::studio) fn is_image_set(&self) -> bool {
        matches!(self, Self::ImageSet(_))
    }

    /// What this session offers the tabs (`SessionCapabilities`): everything for a project,
    /// no chapter- or title-level feature for an image set. Pushed once by `StudioApp::new`.
    #[must_use]
    pub(in crate::studio) fn capabilities(&self) -> SessionCapabilities {
        match self {
            Self::Project(_) => SessionCapabilities::PROJECT,
            Self::ImageSet(_) => SessionCapabilities::IMAGE_SET,
        }
    }

    /// Whether the session has changes the user would lose by closing. Project: the cached
    /// unsaved flag. Image set: the image differs from its last write or a write is in flight;
    /// never while the DISCARD latch is set (the window is closing without saving).
    #[must_use]
    pub(in crate::studio) fn has_unsaved_changes(&self, writers: &StagingWriters, typing: &TypingTabState, ps_editor: &PsEditorTabState) -> bool {
        match self {
            Self::Project(project) => project.has_unsaved_changes(),
            Self::ImageSet(image_set) => !writers.is_discarding() && (image_set.is_dirty(writers.gate(), typing, ps_editor) || image_set.write_in_flight()),
        }
    }

    /// Whether `tab` exists in this session (`image_set::tab_visible`: an image set hides the
    /// chapter- and title-scoped tabs).
    #[must_use]
    pub(in crate::studio) fn tab_visible(&self, tab: AppTab) -> bool {
        image_set::tab_visible(tab, self.is_image_set())
    }

    /// Hotkeys only this session kind registers (after the code-declared ones, before the user
    /// overrides): the image set's save / save-as; none for a project.
    #[must_use]
    pub(in crate::studio) fn hotkey_specs(&self) -> Vec<HotkeySpecV2> {
        match self {
            Self::Project(_) => Vec::new(),
            Self::ImageSet(_) => image_set::hotkey_specs().into(),
        }
    }

    /// The session's persistence jobs for the busy predicates (`studio/busy.rs`); all clear for an
    /// image set, which runs none of them.
    #[must_use]
    pub(in crate::studio) fn busy_jobs(&self) -> PersistenceJobs {
        match self {
            Self::Project(project) => project.busy_jobs(),
            Self::ImageSet(_) => PersistenceJobs::default(),
        }
    }

    /// The close action of a running DISCARD delete job (project only).
    #[must_use]
    pub(in crate::studio) fn exit_cleanup_action(&self) -> Option<PendingCloseAction> {
        match self {
            Self::Project(project) => project.exit_cleanup_action(),
            Self::ImageSet(_) => None,
        }
    }

    /// Returns and clears the request to rebuild the app from disk (set after a page op; never for
    /// an image set).
    pub(in crate::studio) fn take_reload_request(&mut self) -> bool {
        match self {
            Self::Project(project) => project.take_reload_request(),
            Self::ImageSet(_) => false,
        }
    }

    /// Returns and clears the window title the session wants (image set: the opened pages, or page
    /// 0's file after «Сохранить как» moved it; never for a project, whose title the window host
    /// sets from the startup target).
    pub(in crate::studio) fn take_title(&mut self) -> Option<String> {
        match self {
            Self::Project(_) => None,
            Self::ImageSet(image_set) => image_set.take_title(),
        }
    }

    /// What a second OS close does while the exit dialog is open. Project: exit without asking
    /// again (the staging survives an exit, the next open resumes it). Image set: keep the dialog —
    /// every image-set close discards its scratch, so exiting would lose the unsaved edits and skip
    /// the dialog's "only Cancel while a write runs" rule.
    #[must_use]
    pub(in crate::studio) fn os_close_policy(&self) -> OsClosePolicy {
        match self {
            Self::Project(_) => OsClosePolicy::SecondCloseExits,
            Self::ImageSet(_) => OsClosePolicy::SecondCloseKeepsDialog,
        }
    }

    /// The bubbles model moved to a new revision: a project latches its cached unsaved flag. An
    /// image set's dirty state is its controller's, so nothing happens there.
    pub(in crate::studio) fn note_unsaved(&mut self) {
        match self {
            Self::Project(project) => project.note_unsaved(),
            Self::ImageSet(_) => {}
        }
    }

    /// Starts a structural page operation (project only). Unreachable for an image set — the Page
    /// Manager tab does not exist there (`tab_visible`) — and refused with a log if it ever is.
    pub(in crate::studio) fn start_page_op(&mut self, op: PageOpKind, parts: &mut SessionParts<'_>) {
        match self {
            Self::Project(project) => project.start_page_op(op, parts),
            Self::ImageSet(_) => {
                runtime_log::log_error(format!("[page_op] {op:?} refused: an image-set session has no structural page operations (the Page Manager tab is hidden)"));
            }
        }
    }

    /// The per-frame session step, every frame whatever tab is active, at its fixed point of
    /// `StudioApp::ui`. Project: `ProjectSession::tick` (save preload, save / page-op / cleanup
    /// polls, unsaved cache, status expiry, save-then-close). Image set: the controller's `tick`
    /// (it is also the one `poll_flatten_to_file` caller the typing tab requires), then its outcome.
    pub(in crate::studio) fn tick(&mut self, ctx: &egui::Context, now: f64, parts: &mut SessionParts<'_>) {
        match self {
            Self::Project(project) => project.tick(ctx, now, parts),
            Self::ImageSet(image_set) => {
                let outcome = image_set.tick(ctx, SaveParts {
                    project: parts.project,
                    typing: &mut *parts.typing,
                    ps_editor: &mut *parts.ps_editor,
                    gate: parts.writers.gate(),
                });
                apply_image_set_outcome(image_set, outcome, parts);
            }
        }
    }

    /// The session's part of the top bar, right of the «to launcher» button: the project's save
    /// button and status line, or the image set's save buttons and status.
    pub(in crate::studio) fn draw_top_bar(&mut self, ui: &mut egui::Ui, parts: &mut SessionParts<'_>) {
        match self {
            Self::Project(project) => project.draw_top_bar(ui, parts),
            Self::ImageSet(image_set) => image_set.draw_top_bar(ui),
        }
    }

    /// Draws the exit dialog for `kind` (the close it would end in, `CloseFlow::dialog`) under
    /// `title` and acts on its buttons. Returns true when a button started a close (save-then-close
    /// or the discard). Project: Cancel / «Сохранить главу» / «Не сохранять»
    /// (`ProjectSession::draw_exit_dialog`). Image set: its own dialog; Save closes through the
    /// controller's `SaveOutcome`, «Не сохранять» closes the discard way at once.
    pub(in crate::studio) fn draw_exit_dialog(&mut self, ctx: &egui::Context, kind: PendingCloseAction, title: &str, parts: &mut SessionParts<'_>) -> bool {
        let incoming_count = parts.close.incoming_count();
        let image_set = match self {
            Self::Project(project) => return project.draw_exit_dialog(ctx, kind, title, parts),
            Self::ImageSet(image_set) => image_set,
        };
        let action = kind;
        let choice = if kind == PendingCloseAction::OpenIncoming {
            image_set::draw_open_incoming_exit_dialog(ctx, incoming_count, image_set.write_in_flight(), image_set.picker_open())
        } else {
            image_set::draw_exit_dialog(ctx, title, image_set.write_in_flight(), image_set.picker_open())
        };
        match choice {
            None => false,
            Some(ExitChoice::Cancel) => {
                parts.close.close_dialog();
                if kind == PendingCloseAction::OpenIncoming {
                    parts.close.decline_incoming();
                }
                false
            }
            Some(ExitChoice::Save) => {
                // The close follows from the controller's `SaveOutcome` (see `tick`).
                image_set.request_save_then(action);
                parts.close.close_dialog();
                true
            }
            Some(ExitChoice::DontSave) => {
                close_image_set_discarding(image_set, action, parts);
                true
            }
        }
    }

    /// The session's own dialogs, drawn right after the exit dialog and the received-images
    /// confirmation: the image set's JPEG options / folder / skipped-files dialogs (and their save
    /// outcome), or the project's page-op progress veil and failure dialog.
    pub(in crate::studio) fn draw_dialogs(&mut self, ctx: &egui::Context, parts: &mut SessionParts<'_>) {
        match self {
            Self::Project(project) => project.draw_page_op_overlays(ctx, parts),
            Self::ImageSet(image_set) => {
                let outcome = image_set.draw_dialogs(ctx);
                apply_image_set_outcome(image_set, outcome, parts);
            }
        }
    }

    /// The image set's save hotkeys (`image_set::HOTKEY_SAVE_IMAGE` / `HOTKEY_SAVE_IMAGE_AS`,
    /// registered only in that mode); a project session does nothing.
    pub(in crate::studio) fn on_save_hotkey(&mut self, ctx: &egui::Context, command_id: &str) {
        let Self::ImageSet(image_set) = self else {
            return;
        };
        match command_id {
            image_set::HOTKEY_SAVE_IMAGE => image_set.request_save(),
            image_set::HOTKEY_SAVE_IMAGE_AS => image_set.request_save_as(ctx),
            other => runtime_log::log_warn(format!("[single_image] '{other}' is not a save hotkey; ignored")),
        }
    }

    /// First step of `StudioApp::shutdown_blocking`. An image set always closes the DISCARD way
    /// (plan D13): its staging is a scratch the window host's teardown deletes right after the app
    /// is dropped, so nothing after this may write into it. No-op when a dialog already took that
    /// path, and for a project (its teardown flushes unless the user discarded).
    pub(in crate::studio) fn before_shutdown(&mut self, writers: &mut StagingWriters, typing: &mut TypingTabState) {
        match self {
            Self::Project(_) => {}
            Self::ImageSet(_) => writers.quiesce_discarding(typing),
        }
    }
}

/// Closes an image-set session without writing the file: abandons a save that has not started
/// writing, quiesces every writer the discard way (D13) and runs `action`.
fn close_image_set_discarding(image_set: &mut ImageSetSession, action: PendingCloseAction, parts: &mut SessionParts<'_>) {
    image_set.abandon_pending();
    parts.writers.quiesce_discarding(parts.typing);
    parts.close.close_dialog();
    parts.close.finalize(action);
}

/// Acts on the end of an image-set save-then-close: close the discard way (the file is written),
/// or show the exit dialog for the same action again.
fn apply_image_set_outcome(image_set: &mut ImageSetSession, outcome: Option<SaveOutcome>, parts: &mut SessionParts<'_>) {
    match outcome {
        None => {}
        Some(SaveOutcome::CloseNow(action)) => close_image_set_discarding(image_set, action, parts),
        Some(SaveOutcome::ReturnToExitDialog(action)) => parts.close.open_dialog(action),
    }
}

#[cfg(test)]
mod tests {
    use super::{OsClosePolicy, SessionCapabilities, SessionMode, StagingWriters};
    use crate::studio::busy::PersistenceJobs;
    use crate::models::autosave_gate::AutosaveGate;
    use crate::models::bubbles_model::BubblesModel;
    use crate::models::clean_overlays_model::CleanOverlaysModel;
    use crate::models::layer_model::layer_doc::LayerDoc;
    use crate::project::ProjectData;
    use crate::project::single_image::{SingleImageScratch, open_image_set};
    use crate::tabs::AppTab;
    use crate::tabs::ps_editor::PsEditorTabState;
    use crate::tabs::typing::TypingTabState;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// Writes a small decodable PNG at `path`.
    fn write_png(path: &Path) {
        image::RgbImage::from_pixel(4, 3, image::Rgb([200, 100, 50])).save(path).expect("write test png");
    }

    /// Real staging writers over empty models on `gate`; their клин dir lives under `dir`.
    fn writers_on(gate: &Arc<AutosaveGate>, dir: &Path) -> StagingWriters {
        let layer_doc = Arc::new(Mutex::new(LayerDoc::new()));
        let bubbles_model = Arc::new(Mutex::new(BubblesModel::new(Vec::new(), dir.join("bubbles"), dir.join("unsaved_bubbles"), Default::default(), Some(Arc::clone(gate)))));
        let clean_overlays_model = Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&[])));
        StagingWriters::spawn(Arc::clone(gate), &layer_doc, &bubbles_model, &clean_overlays_model, dir.join("unsaved_clean_layers"))
    }

    /// A project chapter (one page, no staging dir) builds a project session that shows every tab,
    /// registers no session hotkeys, runs no job, starts clean and latches unsaved on a bubbles
    /// revision.
    #[test]
    fn a_project_chapter_builds_a_project_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let chapter = dir.path().join("Title").join("Chapter 1");
        std::fs::create_dir_all(chapter.join("src")).expect("chapter dir");
        write_png(&chapter.join("src").join("001.png"));
        let project = ProjectData::load(&chapter, &Value::Null).expect("the fixture chapter loads");
        let mut mode = SessionMode::from_project(&project, &Value::Null, false);
        assert!(!mode.is_image_set());
        assert!(matches!(mode, SessionMode::Project(_)));
        assert_eq!(mode.capabilities(), SessionCapabilities::PROJECT, "a project offers every tab feature");
        for tab in AppTab::ALL {
            assert!(mode.tab_visible(tab), "{tab:?} must exist in a project session");
        }
        assert!(mode.hotkey_specs().is_empty());
        assert_eq!(mode.os_close_policy(), OsClosePolicy::SecondCloseExits, "a project's staging survives an exit");
        assert_eq!(mode.busy_jobs(), PersistenceJobs::default(), "a fresh session runs no job");
        assert_eq!(mode.exit_cleanup_action(), None);
        assert!(!mode.take_reload_request());

        let gate = Arc::new(AutosaveGate::new());
        let mut writers = writers_on(&gate, &dir.path().join("writers"));
        let typing = TypingTabState::default();
        let ps_editor = PsEditorTabState::default();
        assert!(!mode.has_unsaved_changes(&writers, &typing, &ps_editor), "no staging dir: nothing unsaved");
        mode.note_unsaved();
        assert!(mode.has_unsaved_changes(&writers, &typing, &ps_editor), "a bubbles revision latches the cached flag");
        let resumed = SessionMode::from_project(&project, &Value::Null, true);
        assert!(resumed.has_unsaved_changes(&writers, &typing, &ps_editor), "a staging dir found at open starts unsaved");
        writers.stop_overlay_autosave_blocking();
    }

    /// An opened image set builds an image-set session: chapter tabs hidden, its two save hotkeys
    /// (save-as first), no project job; dirty follows the autosave gate, and the DISCARD latch
    /// silences it (the window is closing without saving).
    #[test]
    fn an_image_set_builds_an_image_set_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("page one.png");
        write_png(&source);
        let scratch = SingleImageScratch::reserve(&dir.path().join("scratch")).expect("reserve scratch");
        let project = open_image_set(&[source], Vec::new(), &scratch, &Value::Null, &mut |_, _| {}, &AtomicBool::new(false)).expect("the fixture image opens");
        let mut mode = SessionMode::from_project(&project, &Value::Null, false);
        assert!(mode.is_image_set());
        let caps = mode.capabilities();
        assert_eq!(caps, SessionCapabilities::IMAGE_SET);
        assert!(!caps.title_documents && !caps.chapter_storage && !caps.clean_scan && !caps.series_watermark_library && !caps.alt_versions && !caps.project_export && !caps.project_settings, "an image set offers no chapter- or title-level feature");
        let hidden = [AppTab::PageManager, AppTab::Characters, AppTab::Terms, AppTab::Notes];
        for tab in AppTab::ALL {
            assert_eq!(mode.tab_visible(tab), !hidden.contains(&tab), "{tab:?} in an image set");
        }
        let ids: Vec<&str> = mode.hotkey_specs().iter().map(|spec| spec.id).collect();
        assert_eq!(ids, vec![super::image_set::HOTKEY_SAVE_IMAGE_AS, super::image_set::HOTKEY_SAVE_IMAGE]);
        assert_eq!(mode.os_close_policy(), OsClosePolicy::SecondCloseKeepsDialog, "an image-set exit discards its scratch");
        assert_eq!(mode.busy_jobs(), PersistenceJobs::default());
        assert_eq!(mode.exit_cleanup_action(), None);
        assert!(!mode.take_reload_request());

        let gate = Arc::new(AutosaveGate::new());
        let mut writers = writers_on(&gate, &dir.path().join("writers"));
        let mut typing = TypingTabState::default();
        let ps_editor = PsEditorTabState::default();
        assert!(!mode.has_unsaved_changes(&writers, &typing, &ps_editor), "a freshly opened image is clean");
        mode.note_unsaved();
        assert!(!mode.has_unsaved_changes(&writers, &typing, &ps_editor), "the bubbles revision is not the image set's dirty source");
        gate.note_action();
        assert!(mode.has_unsaved_changes(&writers, &typing, &ps_editor), "an edit moves the gate's action count");
        mode.before_shutdown(&mut writers, &mut typing);
        assert!(writers.is_discarding(), "an image set always closes the discard way");
        assert!(!mode.has_unsaved_changes(&writers, &typing, &ps_editor), "never dirty while discarding");
        writers.stop_overlay_autosave_blocking();
        drop(mode);
        drop(project);
        scratch.remove().expect("remove scratch");
    }
}
