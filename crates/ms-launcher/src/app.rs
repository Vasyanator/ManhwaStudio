/*
File: crates/ms-launcher/src/app.rs

Purpose:
The launcher SCREEN: a plain state machine the host window draws each frame and asks for its
exit (`take_outcome`). It owns no window: it never closes, maximizes or styles the root
viewport.

Main responsibilities:
- gather everything an entry needs that costs blocking I/O, off the GUI thread
  (`prepare_entry` -> `LauncherEntry`), and assemble the screen from it without I/O
  (`LauncherApp::new`);
- own launcher shell state;
- drive background image plan generation and lazy batch decoding;
- render the animated multi-column background with a separate post-image blur layer plus the central menu card;
- poll the main-menu "Open image" picker worker and turn its pick into `LauncherOutcome::OpenImages`;
- start the settings-warnings checks on the first frame of each launcher entry, poll them every
  frame, and route launcher-side setting changes (first-run language confirm) to their recheck;
- hold the one pending exit (`LauncherOutcome`) until the host takes it.

Key items:
- `LauncherEntry`, `prepare_entry`, `prepare_entry_with_settings`
- `LauncherApp::{new, show, take_outcome, set_update_notification}`

Notes:
- the secondary windows (New Project with its batch-processing child, PSD import, Wiki guide)
  are `ms_widgets::EmbeddedWindow`s: one widget decides separate OS window vs embedded, and while
  an embedded window covers the launcher viewport (`covers_host`) the launcher draws nothing of
  its own (`HostPassPlan`) but still polls its workers and still shows its windows;
- every separate-window viewport carries the main window's identity (`with_window_identity`:
  app id + icon from `ms_window_geometry::identity`) so taskbars group it with the main window.
*/

use ms_settings_ui::ai_backend_supervisor::AiBackendHandle;
use ms_config as config;
use crate::background::{
    self, BackgroundImageLoadRequest, BackgroundImagePlan, LoadedBackgroundImage,
};
use crate::first_run_language::FirstRunLanguageState;
use crate::main_page;
use crate::new_project::window::NewProjectWindowState;
use crate::pages::base::{self, PageLayer, PageNavAction};
use crate::pages::export_page::ExportPageState;
use crate::pages::import_page::ImportPageState;
use crate::pages::open_page::OpenPageState;
use crate::pages::settings_page::SettingsPageState;
use ms_settings_ui::settings_warnings::{SettingChange, WarningLevel};
use crate::psd_import_window::PsdImportWindowState;
use crate::state::{LauncherHost, LauncherOutcome, LauncherPage, LauncherState, UpdateNotification};
use crate::theme::VEIL_TINT;
use ms_tabs_simple::wiki::WikiTabState;
#[cfg(feature = "tutorial")]
use ms_settings_ui::tutorial::{TutorialController, TutorialId, TutorialProgressHandle, TutorialStep};
use eframe::egui::{self, epaint};
use ms_widgets::embedded_window::{self, EmbeddedWindow, EmbeddedWindowState};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use web_time::{Duration, Instant};

const BACKGROUND_COLUMNS: usize = 5;
const BACKGROUND_SPACING: f32 = 74.0;
const BACKGROUND_SPEED_PX_PER_SEC: f32 = 22.0;
const BACKGROUND_TILT_DEGREES: f32 = 4.0;
const BACKGROUND_FADE_IN_SECS: f32 = 1.0;
const BACKGROUND_PLACEHOLDER_FILL: egui::Color32 = egui::Color32::TRANSPARENT;
const BACKGROUND_PLACEHOLDER_STROKE: egui::Color32 = egui::Color32::TRANSPARENT;
/// `EmbeddedWindow` identities (stack entry, content `Ui` and separate viewport): never shown.
const NEW_PROJECT_WINDOW_ID: &str = "launcher_new_project_window";
const PSD_IMPORT_WINDOW_ID: &str = "launcher_psd_import_window";
const WIKI_GUIDE_WINDOW_ID: &str = "launcher_wiki_guide_window";

struct BackgroundSlot {
    path: PathBuf,
    column: usize,
    base_y: f32,
    render_height: f32,
    blur_padding: f32,
    texture: Option<egui::TextureHandle>,
    loaded_at: Option<Instant>,
}

struct PendingBackgroundImage {
    rx: Receiver<Option<LoadedBackgroundImage>>,
}

/// Everything one launcher entry (program start or return from the studio) needs that costs
/// blocking I/O, gathered OFF the GUI thread by [`prepare_entry`]; [`LauncherApp::new`] turns it
/// into the screen without further I/O. `Send`, so a worker can build it and hand it over.
///
/// The constructors of the settings page (config reads, locale scan), the new-project window
/// (config reads that may persist migrated defaults, browser and waifu2x probes) and the
/// first-run modal (raw config read, locale scan) perform that I/O, so the entry carries those
/// sub-states already built rather than the raw reads.
pub struct LauncherEntry {
    /// Projects root of this entry, derived from [`Self::user_settings`].
    pub projects_root: PathBuf,
    /// User settings of this entry (stored values over defaults), read when it was prepared.
    pub user_settings: Value,
    /// `--test-launcher`: child windows carry the test app id.
    pub test_mode: bool,
    first_run_language: Option<FirstRunLanguageState>,
    settings_page: SettingsPageState,
    new_project_window: NewProjectWindowState,
    #[cfg(feature = "tutorial")]
    tutorial_progress: TutorialProgressHandle,
    background_decode_parallelism: usize,
}

impl std::fmt::Debug for LauncherEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LauncherEntry")
            .field("projects_root", &self.projects_root)
            .field("test_mode", &self.test_mode)
            .field("first_run_language", &self.first_run_language.is_some())
            .finish_non_exhaustive()
    }
}

// The host prepares the entry on a worker and moves it to the GUI thread.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<LauncherEntry>();
};

/// Prepares one launcher entry. BLOCKING (config reads, disk scans, probes): call it on a
/// worker, never on the GUI thread.
///
/// The user settings are read FRESH, so a change made during the previous entry (for example
/// the projects folder chosen on the settings page) is seen by this one; when that read fails,
/// the failure is logged and `fallback_user_settings` (the host's startup snapshot) is used.
/// `ai_backend` and `host` go to the settings page; `test_mode` is `--test-launcher`.
#[must_use]
pub fn prepare_entry(fallback_user_settings: &Value, ai_backend: AiBackendHandle, host: LauncherHost, test_mode: bool) -> LauncherEntry {
    let user_settings = fresh_user_settings(config::load_user_settings_for_startup(), fallback_user_settings);
    prepare_entry_with_settings(user_settings, ai_backend, host, test_mode)
}

/// [`prepare_entry`] with caller-supplied user settings instead of the fresh config read (the
/// web build roots the launcher at its virtual projects folder). Equally BLOCKING: it still
/// reads the raw config for the first-run language modal and builds the sub-states whose
/// constructors do I/O.
#[must_use]
pub fn prepare_entry_with_settings(user_settings: Value, ai_backend: AiBackendHandle, host: LauncherHost, test_mode: bool) -> LauncherEntry {
    let projects_root = config::projects_root_from_user_settings(&user_settings);
    // First-run language modal: shown only when the RAW (unmerged) config lacks the interface
    // and/or typesetting language. A config read error is bounded degradation — log it and
    // never block the user behind the modal.
    let first_run_language = match config::load_raw_user_settings_for_startup() {
        Ok(raw) => FirstRunLanguageState::from_startup(&raw),
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[launcher] could not read raw settings for the first-run language modal; \
                 skipping it; error={err:#}"
            ));
            None
        }
    };
    // One shared progress handle: the controllers autoplay against it and the settings
    // "Обучение" pane resets it, so all of them stay in sync within the entry.
    #[cfg(feature = "tutorial")]
    let tutorial_progress = ms_settings_ui::tutorial::shared_progress();
    let ai_install_type = config::AiInstallType::from_user_settings(&user_settings);
    let settings_page = SettingsPageState::new(
        projects_root.clone(),
        ai_install_type,
        ai_backend,
        host,
        #[cfg(feature = "tutorial")]
        tutorial_progress.clone(),
    );
    let new_project_window = NewProjectWindowState::new(
        projects_root.clone(),
        #[cfg(feature = "tutorial")]
        tutorial_progress.clone(),
    );
    // `available_parallelism` reads cgroup limits on Linux, so it is taken here too.
    let background_decode_parallelism = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    ms_log::runtime_log::log_info(format!(
        "[launcher] entry prepared{}: projects root '{}', first-run modal {}",
        if test_mode { " (test mode)" } else { "" },
        projects_root.display(),
        if first_run_language.is_some() { "pending" } else { "not needed" }
    ));
    LauncherEntry {
        projects_root,
        user_settings,
        test_mode,
        first_run_language,
        settings_page,
        new_project_window,
        #[cfg(feature = "tutorial")]
        tutorial_progress,
        background_decode_parallelism,
    }
}

/// The user settings an entry runs with: the fresh `read` when it succeeded, else (logged)
/// the host's `fallback` snapshot.
fn fresh_user_settings(read: anyhow::Result<Value>, fallback: &Value) -> Value {
    match read {
        Ok(settings) => settings,
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[launcher] could not re-read user settings for this launcher entry; using the \
                 startup snapshot (a projects folder changed since startup may be stale); error={err:#}"
            ));
            fallback.clone()
        }
    }
}

/// Records `outcome` in `slot` unless an exit is already pending: the FIRST exit of an entry
/// wins, because the host acts on it and a later one in the same frame would contradict what
/// the user saw happen first. A dropped later outcome is logged. Returns whether it was kept.
fn keep_first_outcome(slot: &mut Option<LauncherOutcome>, outcome: LauncherOutcome) -> bool {
    if let Some(pending) = slot {
        ms_log::runtime_log::log_warn(format!("[launcher] exit {pending:?} is already pending; dropping the later exit {outcome:?}"));
        return false;
    }
    *slot = Some(outcome);
    true
}

/// Stamps the main window's identity (app id + icon, `ms_window_geometry::identity`) on the
/// separate-window viewport of a launcher secondary window (batch processing included) so the
/// desktop groups it with the main window.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn with_window_identity(builder: egui::ViewportBuilder, test_mode: bool) -> egui::ViewportBuilder {
    ms_window_geometry::identity::with_identity(builder, test_mode)
}

/// Web twin of the native `with_window_identity`: the browser has no OS windows (and the
/// web build no `ms-window-geometry`), so the builder passes through unchanged.
#[cfg(target_arch = "wasm32")]
pub(crate) fn with_window_identity(builder: egui::ViewportBuilder, _test_mode: bool) -> egui::ViewportBuilder {
    builder
}

/// The launcher screen. Built per entry from a [`LauncherEntry`]; drawn by the host with
/// [`LauncherApp::show`]; its exit is collected with [`LauncherApp::take_outcome`].
pub struct LauncherApp {
    pub state: LauncherState,
    /// `--test-launcher`: child windows carry the test app id.
    test_mode: bool,
    new_project_window: NewProjectWindowState,
    psd_import_window: PsdImportWindowState,
    wiki_guide_tab: WikiTabState,
    /// The Wiki guide window (opened from the New Project window).
    wiki_guide_window: EmbeddedWindowState,
    open_page: OpenPageState,
    import_page: ImportPageState,
    export_page: ExportPageState,
    settings_page: SettingsPageState,
    /// Main-menu onboarding overlay for this launcher viewport. Shares its
    /// progress handle with `settings_page` so a reset in the "Обучение" tab is
    /// seen by autoplay when returning to the main page. `pub(crate)` so
    /// `main_page` can record target rects via `mark`.
    #[cfg(feature = "tutorial")]
    pub(crate) tutorial: TutorialController<LauncherState>,
    /// Last-seen page, to edge-trigger autoplay only on entering the main page.
    #[cfg(feature = "tutorial")]
    tutorial_prev_page: Option<LauncherPage>,
    /// The pending exit of this entry, until the host takes it (`take_outcome`).
    outcome: Option<LauncherOutcome>,
    pub(crate) update_notification: Option<UpdateNotification>,
    /// First-run language-selection modal. `Some` until the user confirms it (or
    /// `None` when both languages were already chosen). While it is `Some` the
    /// main-menu tutorial autoplay is suppressed; on confirm the tutorial is handed
    /// off exactly once. See `first_run_language.rs`.
    first_run_language: Option<FirstRunLanguageState>,
    /// `state.project_creator_open()` of the previous frame: its falling edge rechecks the
    /// projects-folder warning (a create or import may have created the folder).
    project_creator_was_open: bool,
    pub(crate) ai_install_type: config::AiInstallType,
    pending_plan: Option<Receiver<BackgroundImagePlan>>,
    /// Pending "Open image" pick (`open_image::spawn_image_picker`); `Some` while the native
    /// dialog is open. Polled each frame, never waited on.
    #[cfg(not(target_arch = "wasm32"))]
    open_image_pick: Option<Receiver<crate::open_image::ImagePickResult>>,
    pending_images: Vec<PendingBackgroundImage>,
    background_plan: Option<BackgroundImagePlan>,
    background_slots: Vec<BackgroundSlot>,
    background_pending_slots: Vec<usize>,
    background_column_heights: [f32; BACKGROUND_COLUMNS],
    background_column_width: u32,
    background_decode_parallelism: usize,
    background_started_at: Instant,
}

impl LauncherApp {
    /// Builds the launcher screen for one entry. Performs NO blocking I/O (everything that
    /// needs it was gathered by [`prepare_entry`]); it only spawns the page workers (catalogue
    /// scans, wiki scan, background plan). No egui context exists yet, so workers that need
    /// one start on the first frame.
    #[must_use]
    pub fn new(entry: LauncherEntry) -> Self {
        let LauncherEntry {
            projects_root,
            user_settings,
            test_mode,
            first_run_language,
            settings_page,
            new_project_window,
            #[cfg(feature = "tutorial")]
            tutorial_progress,
            background_decode_parallelism,
        } = entry;
        let ai_install_type = config::AiInstallType::from_user_settings(&user_settings);
        let mut app = Self {
            state: LauncherState::new(),
            test_mode,
            new_project_window,
            psd_import_window: PsdImportWindowState::new(projects_root.clone()),
            wiki_guide_tab: WikiTabState::new(),
            wiki_guide_window: EmbeddedWindowState::new(),
            open_page: OpenPageState::new(projects_root.clone(), &user_settings),
            import_page: ImportPageState::new(projects_root.clone()),
            export_page: ExportPageState::new(projects_root.clone()),
            settings_page,
            #[cfg(feature = "tutorial")]
            tutorial: TutorialController::new(
                tutorial_progress,
                vec![(
                    TutorialId::LauncherMain,
                    crate::tutorial::steps
                        as fn() -> Vec<TutorialStep<LauncherState>>,
                )],
            )
            // The launcher's whole look is its animated wallpaper; a lighter dim
            // keeps it visible while still focusing the highlighted button. The
            // callout then needs its own ~70% opaque backing so its text stays
            // readable over that visible backdrop.
            .with_dim_alpha(110)
            .with_callout_tint(egui::Color32::from_rgba_unmultiplied(18, 20, 26, 179)),
            #[cfg(feature = "tutorial")]
            tutorial_prev_page: None,
            outcome: None,
            update_notification: None,
            first_run_language,
            project_creator_was_open: false,
            ai_install_type,
            pending_plan: None,
            #[cfg(not(target_arch = "wasm32"))]
            open_image_pick: None,
            pending_images: Vec::new(),
            background_plan: None,
            background_slots: Vec::new(),
            background_pending_slots: Vec::new(),
            background_column_heights: [0.0; BACKGROUND_COLUMNS],
            background_column_width: 0,
            background_decode_parallelism,
            background_started_at: Instant::now(),
        };
        app.pending_plan = Some(background::spawn_background_plan(projects_root));
        app
    }

    /// Takes the pending exit of this entry, if any (one-shot: a second call returns `None`).
    /// The host calls it after [`Self::show`] each frame and acts on `Some` (open the studio,
    /// start the update); the launcher itself never closes the window.
    pub fn take_outcome(&mut self) -> Option<LauncherOutcome> {
        self.outcome.take()
    }

    /// Sets (`Some`) or clears (`None`) the update notice of the main page. The host owns the
    /// startup update check and hands its result in, so a return from the studio shows the
    /// notice again without a new check.
    pub fn set_update_notification(&mut self, notification: Option<UpdateNotification>) {
        self.update_notification = notification;
    }

    fn poll_workers(&mut self, ctx: &egui::Context, target_width: u32, viewport_height: f32) {
        // Settings warnings: the first frame of this entry starts the full check run
        // (`new` has no egui context); every frame then forwards landed AI-pane writes
        // and applies results, whichever page is shown.
        self.settings_page.ensure_warning_checks_started(ctx);
        // Creating or importing a project may create the projects folder, which no config
        // write reports: recheck it once the creating surface (import page, new-project or
        // PSD-import window) closes. One edge covers every create path.
        let creator_open = self.state.project_creator_open();
        if self.project_creator_was_open && !creator_open {
            self.settings_page.recheck_warnings(&[SettingChange::ProjectsRoot]);
        }
        self.project_creator_was_open = creator_open;
        self.settings_page.poll_warnings(ctx);
        #[cfg(not(target_arch = "wasm32"))]
        self.poll_open_image_pick(ctx);
        self.poll_plan(ctx, target_width, viewport_height);
        self.poll_images(ctx);
        self.kick_background_load(target_width, viewport_height);
    }

    /// The worst settings warning level (badge of the main-menu Settings button); `None`
    /// when no setting is flagged or the checks have not reported yet.
    pub(crate) fn settings_warning_level(&self) -> Option<WarningLevel> {
        self.settings_page.overall_warning_level()
    }

    /// Starts the "Open image" picker worker unless one is already pending. While it is pending
    /// the main page shows the picking status instead of `main_page_message`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn start_open_image_pick(&mut self) {
        if self.open_image_pick.is_some() {
            return;
        }
        ms_log::runtime_log::log_info("[launcher-open-image] picker started");
        self.state.main_page_message = None;
        self.open_image_pick = Some(crate::open_image::spawn_image_picker());
    }

    /// Whether the "Open image" dialog is currently open (the button is disabled meanwhile).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_image_pick_active(&self) -> bool {
        self.open_image_pick.is_some()
    }

    /// Applies a finished pick: a pick with at least one valid file becomes the pending exit
    /// `LauncherOutcome::OpenImages`; a cancel clears the status; a rejection or a dead worker
    /// stays in the launcher with a localized status (details in the log).
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_open_image_pick(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.open_image_pick else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // `spawn_image_picker` already logged why the worker is gone.
                self.open_image_pick = None;
                self.state.main_page_message = Some(t!("launcher.main.open_image_picker_error").to_owned());
                return;
            }
        };
        self.open_image_pick = None;
        match crate::open_image::launcher_outcome_for_pick(result) {
            Ok(None) => self.state.main_page_message = None,
            Ok(Some(outcome)) => {
                ms_log::runtime_log::log_info(format!("[launcher-open-image] leaving the launcher to open {outcome:?}"));
                keep_first_outcome(&mut self.outcome, outcome);
            }
            Err(rejection) => {
                ms_log::runtime_log::log_warn(rejection.log_message());
                self.state.main_page_message = Some(rejection.user_message());
            }
        }
        ctx.request_repaint();
    }

    fn poll_plan(&mut self, ctx: &egui::Context, target_width: u32, viewport_height: f32) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_plan {
            match rx.try_recv() {
                Ok(plan) => {
                    should_clear = true;
                    self.background_plan = Some(plan);
                    self.rebuild_background_layout(ctx, target_width, viewport_height);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear {
            self.pending_plan = None;
        }
    }

    fn poll_images(&mut self, ctx: &egui::Context) {
        let pending = std::mem::take(&mut self.pending_images);
        let mut remaining = Vec::with_capacity(pending.len());
        for pending_image in pending {
            match pending_image.rx.try_recv() {
                Ok(Some(image)) => {
                    self.append_loaded_background_image(ctx, image);
                    ctx.request_repaint();
                }
                Ok(None) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => remaining.push(pending_image),
            }
        }
        self.pending_images = remaining;
    }

    fn append_loaded_background_image(
        &mut self,
        ctx: &egui::Context,
        image: LoadedBackgroundImage,
    ) {
        let LoadedBackgroundImage {
            slot_index,
            path,
            blur_image,
        } = image;
        let blur_texture = ctx.load_texture(
            format!("launcher-bg-blur-{}", path.display()),
            blur_image,
            egui::TextureOptions::LINEAR,
        );
        let Some(slot) = self.background_slots.get_mut(slot_index) else {
            return;
        };
        slot.texture = Some(blur_texture);
        slot.loaded_at = Some(Instant::now());
    }

    fn rebuild_background_layout(
        &mut self,
        ctx: &egui::Context,
        target_width: u32,
        viewport_height: f32,
    ) {
        self.background_slots.clear();
        self.background_pending_slots.clear();
        self.pending_images.clear();
        self.background_column_heights = [0.0; BACKGROUND_COLUMNS];

        if let Some(plan) = self.background_plan.clone() {
            self.build_background_layout(plan, target_width);
        }

        self.kick_background_load(target_width, viewport_height);
        ctx.request_repaint();
    }

    fn kick_background_load(&mut self, target_width: u32, viewport_height: f32) {
        if target_width == 0 || viewport_height <= 0.0 {
            return;
        }

        while self.pending_images.len() < self.background_decode_parallelism {
            let Some(slot_index) = self.select_next_background_slot(viewport_height) else {
                break;
            };
            let slot = &self.background_slots[slot_index];
            self.pending_images.push(PendingBackgroundImage {
                rx: background::spawn_background_image_load(BackgroundImageLoadRequest {
                    slot_index,
                    path: slot.path.clone(),
                    target_width,
                }),
            });
        }
    }

    fn build_background_layout(&mut self, plan: BackgroundImagePlan, target_width: u32) {
        let mut column_offsets = [0.0; BACKGROUND_COLUMNS];

        for source in plan.entries {
            let Some((_, _, render_height, render_blur_padding)) =
                background::background_render_metrics(
                    target_width,
                    source.source_width,
                    source.source_height,
                )
            else {
                continue;
            };
            let column = shortest_column_index(&column_offsets);
            let base_y = column_offsets[column];
            let slot_index = self.background_slots.len();
            self.background_slots.push(BackgroundSlot {
                path: source.path,
                column,
                base_y,
                render_height,
                blur_padding: render_blur_padding,
                texture: None,
                loaded_at: None,
            });
            self.background_pending_slots.push(slot_index);
            column_offsets[column] += render_height + BACKGROUND_SPACING;
        }

        self.background_column_heights = column_offsets;
    }

    fn select_next_background_slot(&mut self, viewport_height: f32) -> Option<usize> {
        let mut best_list_index = None;
        let mut best_score = None;
        for (list_index, slot_index) in self.background_pending_slots.iter().copied().enumerate() {
            let Some(score) = self.background_slot_priority(slot_index, viewport_height) else {
                continue;
            };
            match best_score {
                Some(current) if score >= current => {}
                _ => {
                    best_score = Some(score);
                    best_list_index = Some(list_index);
                }
            }
        }

        best_list_index.map(|list_index| self.background_pending_slots.swap_remove(list_index))
    }

    fn background_slot_priority(
        &self,
        slot_index: usize,
        viewport_height: f32,
    ) -> Option<(u8, i32, usize)> {
        let slot = self.background_slots.get(slot_index)?;
        let column_total_height = self.background_column_heights[slot.column];
        if column_total_height <= 0.0 {
            return Some((2, i32::MAX, slot_index));
        }

        let offset = self.background_column_offset(slot.column, column_total_height);
        let can_repeat = column_total_height > viewport_height + BACKGROUND_SPACING;
        let cycle_origins = if can_repeat {
            [
                -column_total_height - offset,
                -offset,
                column_total_height - offset,
            ]
        } else {
            [-column_total_height - offset, -offset, f32::NAN]
        };

        let viewport_center = viewport_height * 0.5;
        let mut best = (2u8, i32::MAX, slot_index);
        for cycle_origin_y in cycle_origins {
            if cycle_origin_y.is_nan() {
                continue;
            }
            let top = cycle_origin_y + slot.base_y - slot.blur_padding;
            let bottom = cycle_origin_y + slot.base_y + slot.render_height + slot.blur_padding;
            let intersects = bottom >= 0.0 && top <= viewport_height;
            let center = (top + bottom) * 0.5;
            let distance = (center - viewport_center).abs().round() as i32;
            let score = if intersects {
                (0, distance, slot_index)
            } else if bottom < 0.0 {
                (1, (-bottom).round() as i32, slot_index)
            } else {
                (1, (top - viewport_height).round() as i32, slot_index)
            };
            if score < best {
                best = score;
            }
        }

        Some(best)
    }

    fn background_column_offset(&self, column: usize, column_total_height: f32) -> f32 {
        let elapsed = self.background_started_at.elapsed().as_secs_f32();
        let direction = if column.is_multiple_of(2) { 1.0 } else { -1.0 };
        (elapsed * BACKGROUND_SPEED_PX_PER_SEC * direction + column as f32 * 50.0)
            .rem_euclid(column_total_height.max(1.0))
    }

    fn draw_background(&self, ui: &mut egui::Ui, rect: egui::Rect) {
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 20));
        if self.background_slots.is_empty() {
            return;
        }
        let column_width = self.background_column_width.max(1) as f32;
        let total_spacing = BACKGROUND_SPACING * (BACKGROUND_COLUMNS as f32 + 1.0);
        let start_x = rect.left()
            + ((rect.width() - (column_width * BACKGROUND_COLUMNS as f32 + total_spacing))
                .max(0.0)
                * 0.5)
            + BACKGROUND_SPACING;

        for column in 0..BACKGROUND_COLUMNS {
            let clip_rect = egui::Rect::from_min_size(
                egui::pos2(
                    start_x + column as f32 * (column_width + BACKGROUND_SPACING),
                    rect.top(),
                ),
                egui::vec2(column_width, rect.height()),
            );

            let column_slots = self
                .background_slots
                .iter()
                .filter(|slot| slot.column == column)
                .collect::<Vec<_>>();
            let Some(column_total_height) = self.column_total_height(column) else {
                continue;
            };

            let offset = self.background_column_offset(column, column_total_height);

            let max_blur_padding = column_slots
                .iter()
                .map(|slot| slot.blur_padding)
                .fold(0.0, f32::max);
            let painter = ui
                .painter()
                .with_clip_rect(clip_rect.expand(max_blur_padding));
            let tilt_center = clip_rect.center();
            let can_repeat = column_total_height > rect.height() + BACKGROUND_SPACING;

            self.paint_column_cycle(
                &painter,
                &column_slots,
                clip_rect,
                column_width,
                tilt_center,
                -column_total_height - offset,
            );
            self.paint_column_cycle(
                &painter,
                &column_slots,
                clip_rect,
                column_width,
                tilt_center,
                -offset,
            );
            if can_repeat {
                self.paint_column_cycle(
                    &painter,
                    &column_slots,
                    clip_rect,
                    column_width,
                    tilt_center,
                    column_total_height - offset,
                );
            }
        }

        // Global veil between the blurred background stack and the UI card.
        ui.painter().rect_filled(rect, 0.0, VEIL_TINT);
    }

    fn draw_pages(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, rect: egui::Rect) {
        self.state.settle_transition_if_finished();
        let layers = self.visible_page_layers(rect.width());
        let mut nav_action = None;
        for layer in layers {
            let mut page_ui = base::make_layer_ui(ui, rect.shrink2(egui::vec2(24.0, 24.0)), layer);
            let action = match layer.page {
                LauncherPage::Main => main_page::show(self, &mut page_ui),
                LauncherPage::OpenProject => self.open_page.show(&mut page_ui),
                LauncherPage::ImportChapter => self.import_page.show(&mut page_ui),
                LauncherPage::ExportChapter => self.export_page.show(&mut page_ui),
                LauncherPage::Settings => self.settings_page.show(&mut page_ui),
            };
            nav_action = nav_action.or(action);
        }

        if let Some(action) = nav_action {
            self.apply_nav_action(ctx, action);
        }
    }

    fn visible_page_layers(&self, viewport_width: f32) -> Vec<PageLayer> {
        if let Some(transition) = &self.state.page_transition {
            transition
                .visible_layers(viewport_width)
                .into_iter()
                .collect()
        } else {
            vec![PageLayer::stationary(self.state.current_page)]
        }
    }

    fn apply_nav_action(&mut self, ctx: &egui::Context, action: PageNavAction) {
        match action {
            PageNavAction::Open(target) => {
                self.close_settings_resources_if_leaving(target);
                self.state.begin_transition(target);
            }
            PageNavAction::OpenNewProjectWindow => self.state.new_project_window_state.open(ctx),
            PageNavAction::BackToMain => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                self.state.begin_transition(LauncherPage::Main);
            }
            PageNavAction::ProjectsRootChanged(projects_root) => {
                self.apply_projects_root(ctx, projects_root);
            }
            PageNavAction::StorageModeChanged(mode) => {
                self.apply_storage_mode(ctx, mode);
            }
            PageNavAction::AiInstallTypeChanged(install_type) => {
                self.ai_install_type = install_type;
                self.settings_page.set_ai_install_type(install_type);
            }
            PageNavAction::OpenProject(selection) => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                if let Err(err) = crate::pages::open_page::persist_last_selection_values(
                    &selection.title,
                    &selection.chapter,
                ) {
                    ms_log::runtime_log::log_warn(format!(
                        "[launcher-open] failed to persist last selection: {err:#}"
                    ));
                }
                keep_first_outcome(&mut self.outcome, LauncherOutcome::OpenProject(selection));
            }
            PageNavAction::StartUpdate => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                keep_first_outcome(&mut self.outcome, LauncherOutcome::StartUpdate);
            }
        }
    }

    fn close_settings_resources_if_leaving(&mut self, target: LauncherPage) {
        if self.state.current_page == LauncherPage::Settings && target != LauncherPage::Settings {
            self.settings_page.close_python_console();
        }
    }

    fn apply_projects_root(&mut self, ctx: &egui::Context, projects_root: PathBuf) {
        self.open_page.set_projects_root(projects_root.clone());
        self.import_page.set_projects_root(projects_root.clone());
        self.export_page.set_projects_root(projects_root.clone());
        self.settings_page.set_projects_root(projects_root.clone());
        self.new_project_window
            .set_projects_root(projects_root.clone());
        self.psd_import_window
            .set_projects_root(projects_root.clone());
        self.pending_plan = Some(background::spawn_background_plan(projects_root));
        self.background_plan = None;
        self.background_slots.clear();
        self.background_pending_slots.clear();
        self.pending_images.clear();
        self.background_column_heights = [0.0; BACKGROUND_COLUMNS];
        ctx.request_repaint();
    }

    /// Hook run after a storage-mode switch from the settings page finished converting the
    /// global and title documents (the docstore default already equals `mode`). Pages that
    /// show format-dependent state refresh here: the open page re-probes the selected
    /// chapter so its format banner compares against the new mode.
    fn apply_storage_mode(&mut self, ctx: &egui::Context, mode: config::StorageMode) {
        ms_log::runtime_log::log_info(format!("[launcher] storage mode is now '{}'; refreshing format-dependent pages", mode.as_config_str()));
        self.open_page.revalidate_selection();
        ctx.request_repaint();
    }

    /// Shows the New Project window (with batch processing as its child window) for this pass.
    /// Its workers are polled on every pass it is open, also while it is covered or collapsed.
    fn draw_new_project_window(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let test_mode = self.test_mode;
        if self.state.new_project_window_state.is_open() {
            self.new_project_window.poll(&ctx);
        }
        let builder = with_window_identity(
            egui::ViewportBuilder::default()
                .with_inner_size([1180.0, 760.0])
                .with_min_inner_size([1000.0, 680.0])
                .with_resizable(true)
                .with_close_button(true)
                .with_minimize_button(true)
                .with_maximize_button(true)
                .with_active(true),
            test_mode,
        );
        // `content` and `children` both drive the one window state; the widget runs them one
        // after the other (never nested), so the two `borrow_mut`s never overlap.
        let response = {
            let window = std::cell::RefCell::new(&mut self.new_project_window);
            EmbeddedWindow::new(NEW_PROJECT_WINDOW_ID, t!("launcher.new_project.window_title"), &mut self.state.new_project_window_state)
                .viewport(builder)
                .collapsible(true)
                .maximize_on_open(true)
                .content_style(Arc::new(ms_theme::base_style()))
                .show(ui, |ui| window.borrow_mut().show_content(ui), |ui| window.borrow_mut().show_children(ui, test_mode))
        };

        if let Some(selection) = self.new_project_window.take_open_project_selection() {
            if let Err(err) = crate::pages::open_page::persist_last_selection_values(&selection.title, &selection.chapter) {
                ms_log::runtime_log::log_warn(format!("[launcher-open] failed to persist last selection after save: {err:#}"));
            }
            keep_first_outcome(&mut self.outcome, LauncherOutcome::OpenProject(selection));
            self.state.new_project_window_state.close();
        } else if response.close_requested {
            self.new_project_window.on_window_closed();
            self.state.new_project_window_state.close();
        }
        if self.new_project_window.take_open_wiki_guide_requested() {
            self.wiki_guide_window.open(&ctx);
        }
    }

    /// Shows the PSD import window for this pass; its workers are polled on every pass it is open.
    fn draw_psd_import_window(&mut self, ui: &mut egui::Ui) {
        if self.state.psd_import_window_state.is_open() {
            self.psd_import_window.poll(ui.ctx());
        }
        let builder = with_window_identity(
            egui::ViewportBuilder::default()
                .with_inner_size([1360.0, 820.0])
                .with_min_inner_size([1120.0, 720.0])
                .with_resizable(true)
                .with_close_button(true)
                .with_minimize_button(true)
                .with_maximize_button(true)
                .with_active(true),
            self.test_mode,
        );
        let window = &mut self.psd_import_window;
        let response = EmbeddedWindow::new(PSD_IMPORT_WINDOW_ID, t!("launcher.psd_import.window_title"), &mut self.state.psd_import_window_state)
            .viewport(builder)
            .collapsible(true)
            .maximize_on_open(true)
            .content_style(Arc::new(ms_theme::base_style()))
            .show(ui, |ui| window.show_content(ui), |_| {});

        if let Some(selection) = self.psd_import_window.take_open_project_selection() {
            if let Err(err) = crate::pages::open_page::persist_last_selection_values(&selection.title, &selection.chapter) {
                ms_log::runtime_log::log_warn(format!("[launcher-open] failed to persist last selection after psd import: {err:#}"));
            }
            keep_first_outcome(&mut self.outcome, LauncherOutcome::OpenProject(selection));
            self.state.psd_import_window_state.close();
        } else if response.close_requested {
            self.state.psd_import_window_state.close();
        }
    }

    /// Shows the Wiki guide window for this pass (Escape closes it while nothing has focus).
    fn draw_wiki_guide_window(&mut self, ui: &mut egui::Ui) {
        let builder = with_window_identity(
            egui::ViewportBuilder::default()
                .with_inner_size([980.0, 760.0])
                .with_min_inner_size([720.0, 540.0])
                .with_resizable(true)
                .with_close_button(true)
                .with_minimize_button(true)
                .with_maximize_button(true)
                .with_active(true),
            self.test_mode,
        );
        let guide = &mut self.wiki_guide_tab;
        let response = EmbeddedWindow::new(WIKI_GUIDE_WINDOW_ID, t!("launcher.wiki_guide.window_title"), &mut self.wiki_guide_window)
            .viewport(builder)
            .close_on_escape(true)
            .show(
                ui,
                |ui| {
                    // The guide's scan worker reports through a channel the tab drains while drawing.
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                    guide.draw(ui);
                },
                |_| {},
            );
        if response.close_requested {
            self.wiki_guide_window.close();
        }
    }

    fn column_total_height(&self, column: usize) -> Option<f32> {
        let height = self
            .background_column_heights
            .get(column)
            .copied()
            .unwrap_or(0.0);
        if height > 0.0 { Some(height) } else { None }
    }

    fn paint_column_cycle(
        &self,
        painter: &egui::Painter,
        slots: &[&BackgroundSlot],
        clip_rect: egui::Rect,
        column_width: f32,
        tilt_center: egui::Pos2,
        cycle_origin_y: f32,
    ) {
        for slot in slots {
            let y = clip_rect.top() + cycle_origin_y + slot.base_y;
            let image_rect = egui::Rect::from_min_size(
                egui::pos2(clip_rect.left(), y),
                egui::vec2(column_width, slot.render_height.max(1.0)),
            );
            let blur_rect = image_rect.expand(slot.blur_padding);
            if blur_rect.bottom() < clip_rect.top() || blur_rect.top() > clip_rect.bottom() {
                continue;
            }
            if let Some(texture) = &slot.texture {
                paint_rotated_image(
                    painter,
                    texture.id(),
                    blur_rect,
                    tilt_center,
                    BACKGROUND_TILT_DEGREES.to_radians(),
                    egui::Color32::from_white_alpha(
                        (slot_fade_alpha(slot.loaded_at) * 255.0).round() as u8,
                    ),
                );
            } else {
                paint_rotated_placeholder(
                    painter,
                    blur_rect,
                    tilt_center,
                    BACKGROUND_TILT_DEGREES.to_radians(),
                );
            }
        }
    }
}

fn slot_fade_alpha(loaded_at: Option<Instant>) -> f32 {
    loaded_at
        .map(|loaded_at| {
            (loaded_at.elapsed().as_secs_f32() / BACKGROUND_FADE_IN_SECS).clamp(0.0, 1.0)
        })
        .unwrap_or(1.0)
}

impl LauncherApp {
    /// Draws one frame of the launcher screen into the host's root `ui` (the whole window
    /// content) and drives its workers. Non-blocking. Never touches the root viewport (close,
    /// size, style); an exit is left for [`Self::take_outcome`].
    pub fn show(&mut self, ui: &mut egui::Ui) {
        // Derive the `Context` (cheap Arc clone) for worker polling and child windows.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let plan = HostPassPlan::new(embedded_window::covers_host(ctx), self.first_run_language.is_some());

        // Workers run on every pass, also while an embedded window covers the launcher.
        let viewport_rect = ctx.content_rect();
        let target_width = calc_column_width(viewport_rect.width());
        if self.background_column_width != target_width {
            self.background_column_width = target_width;
            if self.background_plan.is_some() {
                self.rebuild_background_layout(ctx, target_width, viewport_rect.height());
            }
        }
        self.poll_workers(ctx, target_width, viewport_rect.height());

        if plan.draw_host {
            self.draw_host(ctx, ui, viewport_rect);
        }

        // The windows are shown on every pass (a window not shown for a pass drops off its
        // stack); the widget decides which of them, if any, draws.
        if plan.show_windows {
            self.draw_new_project_window(ui);
            self.draw_psd_import_window(ui);
            self.draw_wiki_guide_window(ui);
        }

        // Overlay last so its full-viewport hitbox occludes the page content and
        // its spotlight/callout sit above everything on the root viewport.
        #[cfg(feature = "tutorial")]
        if plan.draw_host {
            self.tutorial.render(ctx);
        }

        // Native: smooth 60 FPS. Web: throttle the forced repaint to reduce
        // sustained GPU load (a WebGL/driver-stability safeguard for the demo).
        #[cfg(not(target_arch = "wasm32"))]
        ctx.request_repaint_after(Duration::from_millis(16));
        #[cfg(target_arch = "wasm32")]
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    /// The launcher's own UI for one pass: tutorial tick, background + pages, first-run modal.
    /// Skipped entirely while an embedded window covers the launcher viewport.
    fn draw_host(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, viewport_rect: egui::Rect) {
        // Tutorial is gated behind the `tutorial` feature (off by default); the
        // controller and its `mark` sites stay compiled but inert without it.
        #[cfg(feature = "tutorial")]
        {
            // Autoplay the main-menu tour once per entry to the main page: on first
            // frame (prev None) and whenever navigation settles back onto Main. Skip
            // marks completed, so it never re-fires unless the user resets it.
            let entering_main = self.state.current_page == LauncherPage::Main
                && self.tutorial_prev_page != Some(LauncherPage::Main);
            self.tutorial_prev_page = Some(self.state.current_page);
            // The first-run language modal and the tutorial are mutually exclusive:
            // while the modal is open the tour must not autoplay. On confirm the modal
            // resets `tutorial_prev_page` to `None` (below), so this same edge re-fires
            // on the next frame with the modal gate now open — a single natural handoff.
            if entering_main && self.first_run_language.is_none() {
                self.tutorial.maybe_autoplay(TutorialId::LauncherMain);
            }
            // Run any pending step `on_enter` before the UI is built, then clear the
            // per-frame target registry so this frame's `mark`s repopulate it.
            self.tutorial.sync(&mut self.state);
            self.tutorial.begin_frame();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                self.draw_background(ui, viewport_rect);
                self.draw_pages(ctx, ui, viewport_rect);
            });

        // First-run language modal: rendered above the page content (its blocker
        // occludes all input beneath) and before the child windows. On confirm it
        // clears itself and hands off to the main-menu tutorial exactly once.
        let first_run_confirmed = self
            .first_run_language
            .as_mut()
            .is_some_and(|modal| modal.show(ctx));
        if first_run_confirmed {
            self.first_run_language = None;
            // The modal persisted the interface language: recheck its catalog warning.
            self.settings_page.recheck_warnings(&[SettingChange::UiLanguage]);
            // Hand off to the tutorial through the normal entering-main edge instead of
            // a direct autoplay: clearing the tracked page makes the edge gate at the
            // top of `ui` re-fire next frame, and only when the current page is really
            // Main and the modal-open gate (`first_run_language.is_none()`) now passes.
            #[cfg(feature = "tutorial")]
            {
                self.tutorial_prev_page = None;
            }
        }
    }
}

/// What the launcher host draws in one pass, decided from two facts only (pure, unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostPassPlan {
    /// The launcher's own UI: main panel (background + pages), first-run modal, tutorial.
    draw_host: bool,
    /// The launcher's secondary windows (New Project, PSD import, Wiki guide).
    show_windows: bool,
}

impl HostPassPlan {
    /// `covered`: an embedded window covers the launcher viewport this pass
    /// (`embedded_window::covers_host`). `first_run_modal_open`: the first-run language modal is
    /// up; no secondary window can legitimately be open on a true first run, so none is shown.
    fn new(covered: bool, first_run_modal_open: bool) -> Self {
        Self { draw_host: !covered, show_windows: !first_run_modal_open }
    }
}

fn calc_column_width(total_width: f32) -> u32 {
    let total_spacing = BACKGROUND_SPACING * (BACKGROUND_COLUMNS as f32 + 1.0);
    ((total_width - total_spacing) / BACKGROUND_COLUMNS as f32)
        .max(60.0)
        .round() as u32
}

fn paint_rotated_image(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    rect: egui::Rect,
    rotation_center: egui::Pos2,
    angle_radians: f32,
    tint: egui::Color32,
) {
    let uv = [
        egui::pos2(0.0, 0.0),
        egui::pos2(1.0, 0.0),
        egui::pos2(1.0, 1.0),
        egui::pos2(0.0, 1.0),
    ];
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];

    let mut mesh = epaint::Mesh::with_texture(texture_id);
    let base = mesh.vertices.len() as u32;
    for (corner, uv) in corners.into_iter().zip(uv) {
        mesh.vertices.push(epaint::Vertex {
            pos: rotate_pos(corner, rotation_center, angle_radians),
            uv,
            color: tint,
        });
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    painter.add(epaint::Shape::mesh(mesh));
}

fn paint_rotated_placeholder(
    painter: &egui::Painter,
    rect: egui::Rect,
    rotation_center: egui::Pos2,
    angle_radians: f32,
) {
    let corners = [
        rotate_pos(rect.left_top(), rotation_center, angle_radians),
        rotate_pos(rect.right_top(), rotation_center, angle_radians),
        rotate_pos(rect.right_bottom(), rotation_center, angle_radians),
        rotate_pos(rect.left_bottom(), rotation_center, angle_radians),
    ];
    painter.add(epaint::Shape::convex_polygon(
        corners.to_vec(),
        BACKGROUND_PLACEHOLDER_FILL,
        egui::Stroke::new(1.0, BACKGROUND_PLACEHOLDER_STROKE),
    ));
}

fn shortest_column_index(column_offsets: &[f32; BACKGROUND_COLUMNS]) -> usize {
    let mut best_index = 0usize;
    let mut best_height = f32::INFINITY;
    for (index, height) in column_offsets.iter().copied().enumerate() {
        if height < best_height {
            best_height = height;
            best_index = index;
        }
    }
    best_index
}

fn rotate_pos(pos: egui::Pos2, center: egui::Pos2, angle_radians: f32) -> egui::Pos2 {
    let delta = pos - center;
    let (sin, cos) = angle_radians.sin_cos();
    egui::pos2(
        center.x + delta.x * cos - delta.y * sin,
        center.y + delta.x * sin + delta.y * cos,
    )
}

#[cfg(test)]
mod tests {
    use super::{HostPassPlan, fresh_user_settings, keep_first_outcome};
    use crate::state::{LauncherOutcome, OpenProjectSelection};
    use ms_docstore::{DocKind, DocRef};
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// Reads a user config document the way the startup path does (`ms_docstore`), but from
    /// the given temp path instead of the real runtime root.
    fn read_temp_config(path: &std::path::Path) -> anyhow::Result<Value> {
        let doc = DocRef::new(path.to_path_buf(), DocKind::UserConfig);
        Ok(ms_docstore::read_value(&doc)?.unwrap_or_else(|| json!({})))
    }

    /// A launcher entry sees the projects folder as stored NOW, not the host's startup
    /// snapshot: the stale-projects-root bug of a return from the studio.
    #[test]
    fn prepare_entry_reads_projects_root_fresh_from_config() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("user_config.json");
        let fresh_root = temp.path().join("fresh_projects");
        std::fs::write(&path, serde_json::to_vec(&json!({"General": {"projects_dir": fresh_root.to_string_lossy()}}))?)?;
        let startup_snapshot = json!({"General": {"projects_dir": "/home/u/stale_projects"}});

        let settings = fresh_user_settings(read_temp_config(&path), &startup_snapshot);

        assert_eq!(ms_config::projects_root_from_user_settings(&settings), fresh_root);
        Ok(())
    }

    /// A failed re-read degrades to the host's snapshot instead of an empty config.
    #[test]
    fn prepare_entry_falls_back_to_startup_snapshot_when_config_is_unreadable() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("user_config.json");
        std::fs::write(&path, b"{ not json")?;
        let startup_snapshot = json!({"General": {"projects_dir": "/home/u/snapshot_projects"}});

        let read = read_temp_config(&path);
        assert!(read.is_err(), "a malformed config must fail to read");
        let settings = fresh_user_settings(read, &startup_snapshot);

        assert_eq!(settings, startup_snapshot);
        assert_eq!(ms_config::projects_root_from_user_settings(&settings), PathBuf::from("/home/u/snapshot_projects"));
        Ok(())
    }

    /// The first exit of an entry is kept; a later one is dropped; taking it is one-shot.
    #[test]
    fn take_outcome_is_one_shot_and_first_exit_wins() {
        let open = LauncherOutcome::OpenProject(OpenProjectSelection {
            project_dir: PathBuf::from("/home/u/projects/title/chapter"),
            title: "title".to_owned(),
            chapter: "chapter".to_owned(),
            resume_unsaved: false,
        });
        let mut slot = None;
        assert!(keep_first_outcome(&mut slot, open.clone()));
        assert!(!keep_first_outcome(&mut slot, LauncherOutcome::StartUpdate));
        assert_eq!(slot.take(), Some(open));
        assert_eq!(slot.take(), None);
        assert!(keep_first_outcome(&mut slot, LauncherOutcome::StartUpdate));
    }

    #[test]
    fn host_pass_plan_hides_the_host_only_while_covered() {
        assert_eq!(HostPassPlan::new(false, false), HostPassPlan { draw_host: true, show_windows: true });
        // Covered: nothing of the launcher draws, but its windows are still shown (the covering
        // one draws, the others keep their stack places).
        assert_eq!(HostPassPlan::new(true, false), HostPassPlan { draw_host: false, show_windows: true });
        // First run: the language modal is part of the host; no window may open over it.
        assert_eq!(HostPassPlan::new(false, true), HostPassPlan { draw_host: true, show_windows: false });
    }
}
