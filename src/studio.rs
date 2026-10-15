/*
FILE OVERVIEW: src/studio.rs
The studio screen's editor root (`StudioApp`): project/model initialization, tab routing, the
per-frame order of `ui`, and the input router (`ms_input::InputRouter`). A screen of the main window (`src/shell/`), not a
window: it implements `eframe::App` only for the `ui` frame entry the shell forwards.

Main structs:
- `StudioApp`: root app state (project, shared models, tab states, residency, input router).
The source-page residency pipeline (decode pool, in-order promotion, budgeted tiled GPU upload,
overlay and page-cache loaders, memory-pressure eviction) is `PageResidency` in `studio/residency/`;
this file only feeds it the tab knowledge it must not have: the active tab's page window and
pixel-inspection state (`active_source_page_window`, `active_canvas_source_pixel_inspection`)
and the tabs' own GPU caches (`Tabs::evict_tab_gpu_caches`), each asked of the tabs through the
`ms_studio_tab::StudioTab` hooks. The canvas `cache_pages` setting is not fed from here:
residency reads it from its owner, `CleanOverlaysModel`.

App/frame flow:
- `new(project, bootstrap, ai_backend)`: builds models, tabs, and the input registry
  from the inputs `StudioBootstrap` (`studio/bootstrap.rs`) pre-read on the open worker; it reads
  no config or document itself, so it can run inside a GUI frame.
- `new`: wires shared models (`bubbles`, `clean_overlays`, `text_mask`, the layer document)
  between tabs, seeds the tabs' exit-only flags from the bootstrap's `user_config` (Translation in
  `TranslationTab::new`, Typing via `load_exit_settings`), and shares the app-global AI backend's
  health snapshot and probe channel (`AiBackendHandle`; the supervisor, not the studio, owns the
  backend process and its probe) with the Translation, Cleaning and Settings tabs.
- `ui` (`eframe::App::ui`, egui 0.36): frame tick (poll workers, run the input router, draw the
  active tab); lends `PageResidency::lend_mut` (page geometry + resident textures) to the drawing tab.
- Tabs: the ten program tabs are `Tabs` (`studio/tabs.rs`; the Translation tab is its host
  `TranslationTab`, which owns the Translation canvas). Order-free per-tab questions (input
  declarations and contexts, dock host, GPU eviction, page window, background poll, committed-tree change) go through the
  `StudioTab` fan-outs of `Tabs`. `ui` builds the
  frame context (`TabFrame`), calls `Tabs::draw` for the active tab, polls the panel dock's dirty
  layouts when that tab hosts the dock, then applies the returned `AppRequest`s
  (`apply_app_request`: page op, page open, settings deep link, characters / terms changed).
- Input: `InputRouter::begin_frame` runs ONCE per frame, after the session tick and before the
  top bar and the tab draw; the session executes its own fired commands right after it
  (`SessionMode::run_fired_commands`), the drawing tab reads the same `InputFrame` (`TabFrame`).
- Persistence busy gates (page op, save, save button, typing export, page manager, veil):
  named predicates over `persistence_activity()` in `studio/busy.rs`.
- Modal prompts (comic type, AI device selection): `ComicTypePrompt` / `AiDevicePrompt` in
  `studio/prompts.rs`; a chosen comic type is applied here (`apply_comic_type_preset`).
- Close: the close intent (exit dialog, close request, received-images question) is `CloseFlow`
  in `studio/close_flow.rs`; `request_os_close` and the dialogs record a `PendingCloseAction` that
  the window host takes through `take_outputs`; `StudioApp` never sends a root `Close` or `Title`,
  never maximizes and never touches `egui::Options`. Teardown: `shutdown_blocking` in `studio/shutdown.rs`.
- Staging writers (layer saver, bubbles saver, клин autosave thread), their autosave gate and the
  DISCARD latch: `StagingWriters` in `studio/staging_writers.rs`, one method per writer sequence
  (spawn, discard quiesce, failed-discard restart, page-op quiesce, teardown drain).
- Session kind: `SessionMode` in `studio/session/`, built once in `new` from `project.session()`.
  Every per-kind behaviour (tab visibility, session hotkeys, dirty state, top bar, exit dialog,
  per-frame tick, session dialogs, page ops, teardown quiesce) is one dispatch call; the project
  flows (save-to-project, page op, DISCARD, unsaved cache) are `ProjectSession`
  (`studio/session/project/`), the image-set save controller is `ImageSetSession`
  (`studio/session/image_set/`). Flows borrow the app through `SessionParts` (`session_parts`).
- View sync between the canvas tabs and the PS editor (shared viewport, page, camera center):
  `ViewSync` in `studio/view_sync.rs`, applied before the active tab draws and published at the end
  of the frame; `view_sync_parts` (over `Tabs::view_tabs`) is the borrow glue.
- Images another app delivered while the session runs: the window host calls
  `request_close_for_incoming` (logic in `studio/incoming_open.rs`): the unsaved-changes dialog
  (`CloseFlow::dialog` = `PendingCloseAction::OpenIncoming`) or a one-line confirmation, ending in
  `PendingCloseAction::OpenIncoming` or a decline (`StudioOutput::IncomingDeclined`).
- Outbound: everything the session asks of its window host (close, reload after a page op,
  received-images decline, window title) is ONE queue, `StudioOutput`, drained by `take_outputs`
  (`studio/studio_output.rs`); the inbound calls are `request_os_close` and `request_close_for_incoming`.

Input actions (`build_input_registry`):
- The shared `ms_input::view` set as the crate declares it: every viewer (canvas, advanced
  recognition, the Cleaning editor window) publishes its own view surface and acts only on input
  focused on it, and the zoom commands need a view focus, so a tab without a viewer keeps its keys.
- The canvas' own set (`ms_canvas::input::declarations`) and the shared brush set
  (`ms_tools::mask_brush::declarations`).
- Declared by the tabs (`StudioTab::input_declarations`: Translation canvas, OCR-mode and panel
  actions), merged in `AppTab::ALL` order.
- Image-set save / save-as (declared only in that mode, `SessionMode::input_declarations`).
- The user's overrides (`StudioBootstrap::hotkey_overrides`), applied last.

Profiling (optional, behind the `profiling` cargo feature):
- `ui` is instrumented with coarse `puffin::profile_scope!` markers around the heavy
  per-frame phases (worker polling, GPU upload, active-tab draw). The puffin_egui profiler
  window is shown while the feature is compiled in. All puffin references are gated behind
  `#[cfg(feature = "profiling")]` so the default build pulls in no puffin and has zero overhead.
  How to run: `cargo run --features profiling`.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::canvas::CanvasUiStatus;
use ms_input::{DeclarationError, Declarations, FrameEnv, HotkeyOverrides, InputRegistry, InputRouter, view};
use serde_json::Value;
use crate::memory_manager::{CacheEvictionRequest, MemoryManager, MemoryPressure};
use crate::models::autosave_gate::AutosaveGate;
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::text_mask_model::TextMaskModel;
use crate::project::{ComicType, ProjectData, save_comic_type_to_project_file};
use crate::page_ops::PageOpKind;
use crate::runtime_log;
use crate::tabs::AppTab;
use crate::tabs::characters::CharactersTabState;
use crate::tabs::cleaning::CleaningTabState;
use crate::tabs::notes::NotesTabState;
use crate::tabs::page_manager::PageManagerTabState;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::settings::SettingsTabState;
use crate::tabs::terms::TermsTabState;
use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use crate::tabs::translation::{TranslationTab, TranslationTabState};
use crate::tabs::typing::TypingTabState;
use crate::tabs::wiki::WikiTabState;
use crate::widgets::panel_dock::persist as panel_dock_persist;
use crate::widgets::panel_dock::{PanelDockState, PanelLayoutWriter};
use eframe::egui;
use ms_thread as thread;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use web_time::Instant;

// Blocking teardown (`StudioApp::shutdown_blocking`) and the permanent `StudioApp: Send` guard.
mod shutdown;
// Every disk read `new` needs, done beforehand on the open worker (`StudioBootstrap`).
mod bootstrap;
pub(crate) use bootstrap::StudioBootstrap;
// Images delivered by another app while the session runs (`request_close_for_incoming`).
mod incoming_open;
// Named "persistence busy" predicates (`PersistenceActivity`) used by every gate in this file.
mod busy;
// Source-page residency (`PageResidency`): decode pool, in-order promotion, GPU upload, eviction.
mod residency;
use residency::PageResidency;
// The comic-type and AI device selection prompts (`ComicTypePrompt`, `AiDevicePrompt`).
mod prompts;
use prompts::{AiDevicePrompt, ComicTypePrompt};
// Shared viewport / page sync between the canvas tabs and the PS editor (`ViewSync`), and the one
// tab -> canvas mapping (`canvas_slot`).
mod view_sync;
use view_sync::{ViewSync, ViewTabs};
// The program-tab states, their per-frame draw and the requests a draw returns (`Tabs`, `AppRequest`).
mod tabs;
use tabs::{AppRequest, TabFrame, Tabs};
// The three staging writers, their shared autosave gate and the DISCARD latch (`StagingWriters`).
mod staging_writers;
use staging_writers::StagingWriters;
// The session's close intent: exit dialog, outbound close request, received-images question.
mod close_flow;
use close_flow::CloseFlow;
// The one outbound queue to the window host (`StudioOutput`, drained by `take_outputs`).
mod studio_output;
pub(crate) use studio_output::StudioOutput;
// The session strategy (`SessionMode`: project chapter or image set) and the parts its flows
// borrow. `image_set` is crate-visible for the window title rule the window host shares.
pub(crate) mod session;
use session::{SessionMode, SessionParts};
// Named by the main window's Closing screen (`src/shell/closing.rs`), which runs the teardown.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use shutdown::ShutdownReport;

pub struct StudioApp {
    project: ProjectData,
    /// Source-page residency: decode pool, in-order promotion, budgeted GPU upload, overlay and
    /// page-cache loaders, page geometry + resident textures lent to the drawing tab, and the
    /// memory-pressure eviction of what it owns (`studio/residency/`).
    residency: PageResidency,
    bubbles_model: Arc<Mutex<BubblesModel>>,
    clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    applied_bubbles_revision: u64,
    active_tab: AppTab,
    /// Shared viewport / page / camera-center sync between the canvas tabs and the PS editor
    /// (`studio/view_sync.rs`), applied before the active tab draws and published after the frame.
    view_sync: ViewSync,
    ai_backend_health: Arc<Mutex<AiBackendHealthSnapshot>>,
    /// Per-frame cached copy of `ai_backend_health`, refreshed once per frame in `update`.
    /// The AI prompt helpers read this snapshot instead of locking and cloning the shared
    /// health state several times per frame.
    ai_backend_health_cached: AiBackendHealthSnapshot,
    ai_backend_probe_tx: Option<Sender<AiBackendProbeCommand>>,
    /// The ten program tabs (`studio/tabs.rs`; Translation owns its canvas); the active one draws
    /// through `Tabs::draw` and asks the app for anything else through `AppRequest`s. Declared
    /// before the panel dock, layer document and staging writers, so every tab state (and its Arc
    /// clones of the shared models) is dropped before them.
    tabs: Tabs,
    /// The dockable-panel arrangement of this studio window: every program tab's
    /// `DockLayout` (keyed by `AppTab::key()`), the detached OS windows they place
    /// panels in, and the dock's last-frame measurements.
    ///
    /// ONE state per studio window, owned here rather than by the tabs that draw it,
    /// because three of its concerns are global and cannot be split per program tab:
    /// * a sub-window's `ViewportId` is derived from its INDEX alone, and the index
    ///   counter is per state — two states would mint two immediate viewports with
    ///   the same id in one pass;
    /// * the persisted `sub_windows` list is global (the writer merges `layouts` per
    ///   key but REPLACES that list), so a second feeder would overwrite the first
    ///   tab's windows on disk;
    /// * installing and pruning sub-windows prunes them against the layouts of THIS
    ///   state, so several states would each drop the other tabs' windows.
    ///
    /// Lent to the drawing tab for the frame (`TypingDrawParams::panel_dock`); the
    /// tab borrows it disjointly from its own state.
    panel_dock: PanelDockState,
    /// The single writer of the `PanelLayout` section of `user_config.json`
    /// (`crates/ms-widgets/src/panel_dock/persist.rs`). One per studio window, owned here
    /// rather than by `panel_dock` itself, because that state is also built in
    /// tests and may neither spawn a thread nor reach the disk. `panel_dock` is
    /// polled for a dirty layout once per frame; the writer coalesces the burst
    /// and writes off the GUI thread.
    panel_layout_writer: PanelLayoutWriter,
    /// Shared unified layer document: the source of truth for per-page layer MODEL state, held by
    /// both view tabs (`Arc<Mutex<…>>`). Each tab re-projects its current page whenever the doc's
    /// `version` changes (the in-memory cross-tab sync). The app polls its saver acks each frame.
    layer_doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    /// The three staging writers (layer saver, bubbles saver, клин autosave thread), their shared
    /// autosave gate and the DISCARD latch (`studio/staging_writers.rs`). Declared right after
    /// `layer_doc` so its model Arc clones are released at the same point of the drop.
    writers: StagingWriters,
    /// The studio's input runtime: the registry (view set + tabs + session + user overrides) and
    /// the per-frame `InputFrame` every consumer reads (`begin_frame` once per frame in `ui`).
    /// Lent to the drawing tab through `TabFrame::input`; the Settings pane edits its bindings.
    input: InputRouter,
    /// "Pages or ribbon?" prompt of a project without a comic type (`studio/prompts.rs`).
    comic_type_prompt: ComicTypePrompt,
    /// AI device selection prompt, opened from the per-frame health snapshot (`studio/prompts.rs`).
    ai_device_prompt: AiDevicePrompt,
    /// Id of the storage-mode reconciliation whose failure notice the user dismissed in the
    /// top bar (see `draw_storage_reconcile_notice`).
    storage_notice_dismissed_job: Option<u64>,
    /// The session strategy (`studio/session/`), chosen once from `project.session()`: a project
    /// chapter (`ProjectSession`: save-to-project, page ops, DISCARD, the unsaved cache) or an image
    /// set (`ImageSetSession`: «Сохранить» / «Сохранить как», its dirty baseline and dialogs).
    session: SessionMode,
    /// The close intent (`studio/close_flow.rs`): the exit dialog shown, the close waiting for the
    /// window host (drained by `take_outputs`), the received-images question and its decline latch.
    /// `StudioApp` never closes the window itself.
    close: CloseFlow,
    /// Whether the puffin_egui frame-profiler window is shown. Compiled in only under the
    /// `profiling` feature; defaults to on so the profiler is visible immediately, and can be
    /// toggled at runtime with F11.
    #[cfg(feature = "profiling")]
    profiler_window_open: bool,
}

/// What happens once the window may close: exit the program, return to the launcher, or open the
/// images another app delivered (the window host holds them). Also keys the exit dialog variant
/// (`CloseFlow::dialog`: `Exit` = the OS close button, `ReturnToLauncher` = the in-app button,
/// `OpenIncoming` = received images) and is carried by the image-set save controller across a
/// save-then-close (`studio/session/image_set/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingCloseAction {
    Exit,
    ReturnToLauncher,
    OpenIncoming,
}

impl StudioApp {
    /// Builds the studio for `project` from the inputs `bootstrap` pre-read for it
    /// (`StudioBootstrap::build_blocking`, run on the open worker): wires the shared models into
    /// the tabs and spawns the session's workers (page residency, staging writers, savers, the
    /// panel-layout writer). Performs no config or document read itself (the bubbles staging probe
    /// is pre-read too: `BubblesModel::new_with_staging_probe`), so it may run inside a GUI frame.
    pub fn new(project: ProjectData, bootstrap: StudioBootstrap, ai_backend: AiBackendHandle) -> Self {
        let local_ai = ai_backend.local_ai;
        // Enable puffin scope recording once at editor startup. Without this the
        // `profile_scope!` markers are no-ops and the profiler window stays empty.
        #[cfg(feature = "profiling")]
        puffin::set_scopes_on(true);
        let StudioBootstrap { user_settings, hotkey_overrides, settings_seed, spellcheck_words, typing_seed, has_staging, has_staged_bubbles } = bootstrap;
        let memory_manager = Arc::new(MemoryManager::new(
            crate::config::memory_profile_from_user_settings(&user_settings),
        ));
        let cache_pages = project.canvas_settings.cache_pages;
        let residency = PageResidency::spawn(&project, cache_pages, Arc::clone(&memory_manager));
        // Warm the process-global text-render FontSystem pool off the GUI thread. The first lease
        // pays a full system-font scan (~2.2s in a fresh process); doing it here on a detached
        // background thread keeps it off the critical path so the user's first text render finds a
        // warm system. Guarded by `Once` because `new` can run multiple times per process
        // (launcher<->studio transitions) and we want at most one prewarm thread.
        spawn_text_render_font_pool_prewarm();
        // The project's canvas settings, normalized exactly as a canvas stores them: the seed of
        // the shared model's settings. No canvas is seeded directly: every canvas (the Translation
        // one included) pulls them from `BubblesModel` by revision on its first draw.
        let shared_canvas_settings = crate::canvas::shared_canvas_settings_from_project(&project.canvas_settings);
        // ONE autosave gate per project instance, shared by every `{chapter}_unsaved` writer: each
        // writer holds its changes in memory and writes them when the gate is due (interval since the
        // first pending change, or the action count, from the live global policy). Force points
        // (barriers, shutdown, `force_flush`) bypass it; see `ARCHITECTURE.md` "Key invariants" (staging and autosave).
        let autosave_gate = Arc::new(AutosaveGate::new());
        let bubbles_model = Arc::new(Mutex::new(BubblesModel::new_with_staging_probe(
            project.bubbles.as_ref().clone(),
            project.paths.bubbles_file.clone(),
            project.paths.unsaved_bubbles_file.clone(),
            shared_canvas_settings,
            Some(Arc::clone(&autosave_gate)),
            has_staged_bubbles,
        )));
        let applied_bubbles_revision = match bubbles_model.lock() {
            Ok(model) => model.revision(),
            Err(err) => {
                // Not expected: the model was created a few lines above and nothing has run under
                // its lock yet. 0 is only a placeholder: while the lock stays poisoned no frame can
                // read the model either (`sync_project_from_bubbles_model` skips it).
                runtime_log::log_error(format!(
                    "[app] BubblesModel lock poisoned while reading its initial revision; using 0; error={err}"
                ));
                0
            }
        };
        // The backend process + health/device probe are owned app-globally by
        // `AiBackendSupervisor` (see `startup::run_native`); the studio just shares its handle so
        // the backend keeps running across launcher<->studio transitions.
        let ai_backend_health = Arc::clone(&ai_backend.health);
        let ai_backend_probe_tx = ai_backend.probe_tx.clone();
        let text_mask_model = Arc::new(Mutex::new(TextMaskModel::new()));
        let mut translation_state = TranslationTabState::new(
            local_ai,
            Arc::clone(&ai_backend_health),
            ai_backend_probe_tx.clone(),
        );
        translation_state.set_text_mask_model(Arc::clone(&text_mask_model));
        // The host owns the Translation canvas (scroll salt, bubbles model, its bottom-hint
        // collapsed flag from `user_settings`); the shared view starts from that canvas' viewport.
        let mut translation_tab = TranslationTab::new(translation_state, Arc::clone(&bubbles_model), &user_settings);
        let shared_canvas_viewport = translation_tab.canvas().viewport_snapshot();
        let mut cleaning_tab = CleaningTabState::default();
        cleaning_tab.set_canvas_scroll_area_id_salt("cleaning_canvas_scroll");
        cleaning_tab.set_bubbles_model(Arc::clone(&bubbles_model));
        // The tabs own the detector-mask model; the app never reads it, so it keeps no clone.
        cleaning_tab.set_text_mask_model(text_mask_model);
        cleaning_tab.set_ai_backend_health(Arc::clone(&ai_backend_health));
        let page_paths: Vec<PathBuf> = project.pages.iter().map(|p| p.path.clone()).collect();
        let clean_overlays_model =
            Arc::new(Mutex::new(CleanOverlaysModel::new_from_pages(&page_paths)));
        if let Ok(mut overlays) = clean_overlays_model.lock() {
            // Seeds the setting's owner from the project value (the canvases copy it from here).
            overlays.set_cache_pages_enabled(cache_pages);
            overlays.set_memory_profile(memory_manager.profile());
            overlays.set_autosave_gate(Some(Arc::clone(&autosave_gate)));
        } else {
            runtime_log::log_error(
                "[app] clean-overlay model lock poisoned at startup: autosave gate not attached, so клин \
                 edits report no autosave actions (they still reach staging at every force point)",
            );
        }
        cleaning_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        let mut typing_tab = TypingTabState::from_seed(typing_seed);
        typing_tab.set_canvas_scroll_area_id_salt("typing_canvas_scroll");
        // The typing tab's exit-only flags (bottom-hint collapsed, both centering-assist checkboxes),
        // seeded once from `user_config`; written back by the teardown (`shutdown.rs`), never per toggle.
        // The Translation tab reads its own flag (`TranslationTab::new`).
        typing_tab.load_exit_settings(&user_settings);
        typing_tab.set_bubbles_model(Arc::clone(&bubbles_model));
        typing_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        let mut ps_editor_tab = PsEditorTabState::default();
        ps_editor_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        // Shared unified layer document, owned at the app level and held by both view tabs. It is the
        // source of truth for per-page layer MODEL state; each tab re-projects its current page when
        // the doc's `version` changes, which is the in-memory cross-tab sync.
        let layer_doc = std::sync::Arc::new(std::sync::Mutex::new(
            crate::models::layer_model::layer_doc::LayerDoc::new(),
        ));
        // Enable the off-thread coalescing layer saver ONCE on the shared doc, right after creation
        // and before either tab gets its clone. Heavy doc-routed flushes (PS per-edit/raster, typing
        // text) then enqueue PNG-encode + manifest RMW off the GUI thread; the save-to-project merge
        // worker and app-close drain barrier the queue so no write is lost. With no saver the doc's
        // enqueue_page_* methods fall back to a synchronous flush, so wiring is safe either way.
        // With the autosave gate the saver HOLDS enqueued jobs until the gate is due; barriers and
        // shutdown still force every held job out.
        if let Ok(mut doc) = layer_doc.lock() {
            doc.enable_background_saver(Some(Arc::clone(&autosave_gate)));
        } else {
            runtime_log::log_error(
                "[app] could not enable background layer saver: shared doc lock poisoned at startup",
            );
        }
        typing_tab.set_layer_doc(std::sync::Arc::clone(&layer_doc));
        ps_editor_tab.set_layer_doc(std::sync::Arc::clone(&layer_doc));
        let mut settings_tab =
            SettingsTabState::from_seed(settings_seed, ai_backend.clone(), Arc::clone(&memory_manager));
        settings_tab.set_canvas_settings_binding(
            project.paths.settings_file.clone(),
            match bubbles_model.lock() {
                Ok(model) => model.canvas_snapshot(),
                Err(err) => {
                    // Not expected (the model was created above); the binding then starts from the
                    // default canvas settings.
                    runtime_log::log_error(format!(
                        "[app] BubblesModel lock poisoned while seeding the Settings canvas binding; using defaults; error={err}"
                    ));
                    Default::default()
                }
            },
            Arc::clone(&bubbles_model),
            Arc::clone(&clean_overlays_model),
            spellcheck_words,
        );
        // The one session-kind decision of `src/studio`: every per-kind behaviour below dispatches on it.
        let session = SessionMode::from_project(&project, &user_settings, has_staging);
        // The ONE push of what the session offers into the tabs that gate a feature on it. Once is
        // enough: the session kind never changes during a `StudioApp` lifetime. The Settings push
        // must follow `set_canvas_settings_binding` (it may clear the project spellcheck target).
        let session_caps = session.capabilities();
        translation_tab.state_mut().set_session_capabilities(session_caps);
        cleaning_tab.set_session_capabilities(session_caps);
        typing_tab.set_session_capabilities(session_caps);
        settings_tab.set_session_capabilities(session_caps);
        let comic_type_prompt_open = project.comic_type.is_none();

        // Start the клин overlay autosave thread; it writes the dirty pages when the gate is due.
        let writers = StagingWriters::spawn(
            autosave_gate,
            &layer_doc,
            &bubbles_model,
            &clean_overlays_model,
            project.paths.unsaved_clean_layers_dir.clone(),
        );
        let mut page_manager_tab = PageManagerTabState::default();
        page_manager_tab.set_bubbles_model(Arc::clone(&bubbles_model));
        page_manager_tab.set_overlays_model(Arc::clone(&clean_overlays_model));
        page_manager_tab.set_layer_doc(Arc::clone(&layer_doc));
        let tabs = Tabs {
            translation: translation_tab,
            cleaning: cleaning_tab,
            typing: typing_tab,
            ps_editor: ps_editor_tab,
            page_manager: page_manager_tab,
            characters: CharactersTabState::default(),
            terms: TermsTabState::default(),
            notes: NotesTabState::default(),
            settings: settings_tab,
            wiki: WikiTabState::new(),
        };
        // The shared view set, every tab's declarations (`AppTab::ALL` order), then the session's
        // own, then the user's overrides.
        let input = InputRouter::new(input_registry_or_empty(build_input_registry(tabs.input_declarations(), session.input_declarations(), &hotkey_overrides)));
        // Dockable-panel arrangement of this studio window, restored from the snapshot already in
        // hand against the dock hosts' default layouts. It must be installed BEFORE the first frame:
        // the dock builds the default layout for any key that has none, and the default is what the
        // restored layout has to win over.
        let panel_dock = restore_panel_dock(&user_settings, &tabs.dock_layout_defaults());

        Self {
            project,
            residency,
            bubbles_model,
            clean_overlays_model,
            applied_bubbles_revision,
            active_tab: AppTab::Translation,
            view_sync: ViewSync::new(shared_canvas_viewport),
            ai_backend_health,
            ai_backend_health_cached: AiBackendHealthSnapshot::default(),
            ai_backend_probe_tx: ai_backend_probe_tx.clone(),
            tabs,
            panel_dock,
            panel_layout_writer: PanelLayoutWriter::spawn(),
            layer_doc,
            writers,
            input,
            comic_type_prompt: ComicTypePrompt::new(comic_type_prompt_open),
            ai_device_prompt: AiDevicePrompt::default(),
            storage_notice_dismissed_job: None,
            session,
            close: CloseFlow::default(),
            #[cfg(feature = "profiling")]
            profiler_window_open: true,
        }
    }

    /// Returns true if there are in-session changes the user would lose by closing
    /// (`SessionMode::has_unsaved_changes`): a project's changes not yet merged into the project
    /// folder; an image set's edits not yet written to its files, or a write still in flight.
    fn has_unsaved_changes(&self) -> bool {
        self.session.has_unsaved_changes(&self.writers, &self.tabs.typing, &self.tabs.ps_editor)
    }

    /// Whether `tab` exists in this session (`SessionMode::tab_visible`).
    fn tab_visible(&self, tab: AppTab) -> bool {
        self.session.tab_visible(tab)
    }

    /// Returns the loaded project's root directory for bootstrap-driven reconstruction.
    #[must_use]
    pub fn project_dir(&self) -> PathBuf {
        self.project.paths.project_dir.clone()
    }

    /// Selects a tab after a full project reload. A tab hidden in this session is refused (logged).
    pub fn set_active_tab(&mut self, tab: AppTab) {
        if !self.tab_visible(tab) {
            runtime_log::log_warn(format!("[app] tab {tab:?} does not exist in this session; staying on {:?}", self.active_tab));
            return;
        }
        self.active_tab = tab;
    }

    /// Starts a structural page operation requested by the Page Manager
    /// (`SessionMode::start_page_op`; the gated sequence is `ProjectSession::start_page_op`).
    fn start_page_op(&mut self, op: PageOpKind) {
        let (session, mut parts) = self.session_parts();
        session.start_page_op(op, &mut parts);
    }

    /// Splits the borrow of `self` into the session strategy and the app parts its flows borrow
    /// (`SessionParts`: project, staging writers, close intent and the tab states they touch).
    fn session_parts(&mut self) -> (&mut SessionMode, SessionParts<'_>) {
        (
            &mut self.session,
            SessionParts {
                project: &self.project,
                writers: &mut self.writers,
                close: &mut self.close,
                typing: &mut self.tabs.typing,
                ps_editor: &mut self.tabs.ps_editor,
                translation_canvas: self.tabs.translation.canvas_mut(),
                active_tab: self.active_tab,
                cleaning: &mut self.tabs.cleaning,
                page_manager: &mut self.tabs.page_manager,
            },
        )
    }

    /// Drains everything this session asks of its window host since the last call, in the order
    /// the host applies it (`studio_output::collect_outputs`: title, close, received-images
    /// decline, reload; a close suppresses a same-drain reload). Empty = keep running unchanged.
    /// The host polls it after every studio frame and wherever it calls an inbound method that
    /// can decide a close (`request_os_close`); GUI thread, no I/O.
    pub(crate) fn take_outputs(&mut self) -> Vec<StudioOutput> {
        studio_output::collect_outputs(self.session.take_title(), self.close.take_close_request(), self.close.take_incoming_declined(), self.session.take_reload_request())
    }

    /// Handles the OS close button (the root window's `close_requested`). The caller has already
    /// cancelled the OS close: with unsaved work and no exit dialog open this opens the dialog
    /// (its Save starts a save-then-close that ends in a close request); with the dialog already
    /// open the session's policy decides (`SessionMode::os_close_policy`: a project exits, an image
    /// set keeps the dialog); otherwise it requests [`PendingCloseAction::Exit`]. A close request
    /// already pending this frame is kept as is.
    pub(crate) fn request_os_close(&mut self) {
        let has_unsaved_changes = self.has_unsaved_changes();
        self.close.request_os_close(has_unsaved_changes, self.session.os_close_policy());
    }

    /// Draw and handle the active exit/leave dialog (`SessionMode::draw_exit_dialog`: the
    /// project's or the image set's).
    /// Returns true when the dialog requested an app exit that was already dispatched.
    fn draw_exit_dialog(&mut self, ctx: &egui::Context) -> bool {
        // `kind` is the close the dialog would end in (`CloseFlow::dialog`).
        let Some(kind) = self.close.dialog() else {
            return false;
        };
        let title = match kind {
            PendingCloseAction::Exit => t!("app.exit.exit_button"),
            PendingCloseAction::ReturnToLauncher => t!("app.exit.to_launcher_button"),
            PendingCloseAction::OpenIncoming => t!("app.exit.open_incoming_title"),
        };
        let (session, mut parts) = self.session_parts();
        session.draw_exit_dialog(ctx, kind, title, &mut parts)
    }

    /// Applies the comic type the prompt returned: records it in the project (in memory and in
    /// its settings file), then applies its canvas preset to the shared canvas settings — the
    /// `BubblesModel` snapshot, through `SharedCanvasSettings::apply_comic_type_preset`, the one
    /// preset owner — and pushes the result to `BubblesModel` (every canvas pulls it), to
    /// `CleanOverlaysModel` (`cache_pages`) and to the Settings tab, in that order. A failed file
    /// write is shown in the prompt and stops before the pushes. GUI thread; one small file write.
    fn apply_comic_type_preset(&mut self, comic_type: ComicType) {
        self.project.comic_type = Some(comic_type);
        if let Some(root) = self.project.settings_data.as_object_mut() {
            root.insert(
                "comic_type".to_string(),
                serde_json::Value::String(comic_type.as_config_str().to_string()),
            );
        }
        if let Some((aside_compact_mode, separate_pages)) = comic_type.canvas_preset() {
            self.project.canvas_settings.aside_compact_mode = aside_compact_mode.to_string();
            self.project.canvas_settings.separate_pages = separate_pages;
        }

        // GUI thread: an atomic replace WITHOUT fsync (CLAUDE.md §5); the synchronous I/O
        // itself is a recorded gap (`dev-docs/known_gaps.md`).
        if let Err(err) = save_comic_type_to_project_file(
            &self.project.paths.settings_file,
            comic_type,
            ms_docstore::Durability::None,
        ) {
            let user_message = t!("app.comic_type.save_error");
            self.comic_type_prompt.fail(user_message.to_string());
            runtime_log::log_error(format!(
                "[app] failed to persist comic_type='{}'; settings_file='{}'; cause={err}",
                comic_type.as_config_str(),
                self.project.paths.settings_file.display()
            ));
            eprintln!(
                "ERROR app::apply_comic_type_preset comic_type={} settings_file={} error={}",
                comic_type.as_config_str(),
                self.project.paths.settings_file.display(),
                err
            );
            return;
        }

        // The preset goes onto the shared settings every canvas pulls from (one lock: read, apply,
        // write back). Without that lock there is no current snapshot to apply it to: the project
        // file already holds the comic type, the live settings keep their values.
        let snapshot = match self.bubbles_model.lock() {
            Ok(mut bubbles_model) => {
                let mut snapshot = bubbles_model.canvas_snapshot();
                snapshot.apply_comic_type_preset(comic_type);
                bubbles_model.set_canvas_settings(snapshot.clone());
                snapshot
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[app] failed to lock BubblesModel while applying comic_type preset; comic_type='{}' saved to the project file, canvas settings unchanged; error={err}",
                    comic_type.as_config_str()
                ));
                eprintln!("WARN app::apply_comic_type_preset: failed to lock BubblesModel");
                self.comic_type_prompt.resolve();
                return;
            }
        };
        if let Ok(mut overlays_model) = self.clean_overlays_model.lock() {
            overlays_model.set_cache_pages_enabled(snapshot.cache_pages);
        } else {
            runtime_log::log_warn(
                "[app] failed to lock CleanOverlaysModel while applying comic_type preset",
            );
            eprintln!("WARN app::apply_comic_type_preset: failed to lock CleanOverlaysModel");
        }
        self.tabs.settings
            .replace_canvas_settings_from_snapshot(snapshot.clone());
        self.tabs.settings.persist_canvas_settings();

        self.comic_type_prompt.resolve();
    }

    /// Refreshes the per-frame AI backend health cache from the shared probe state.
    ///
    /// Must be called once per frame before any helper that reads
    /// `ai_backend_health_cached`, so the shared mutex is locked and cloned at most once
    /// per frame instead of per consumer.
    fn refresh_ai_backend_health_cache(&mut self) {
        self.ai_backend_health_cached = match self.ai_backend_health.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
    }

    /// Switches to a page-capable tab and applies the selected page without letting viewport
    /// snapshot synchronization overwrite the explicit focus ([`ViewSync::open_page`] makes the
    /// destination the viewport owner, so the next [`ViewSync::before_draw`] skips its re-apply).
    /// Refused (logged) for a tab this session does not show.
    fn open_page_in_tab(&mut self, tab: AppTab, page_idx: usize) {
        if !self.tab_visible(tab) {
            runtime_log::log_warn(format!("[app] open page {page_idx} in {tab:?} refused: the tab does not exist in this session"));
            return;
        }
        self.active_tab = tab;
        // Center of the page in page-local source pixels when its metadata is already loaded.
        // `None` defers the center to the canvas: `CanvasView::focus_page` keeps the request
        // pending and resolves the center from `PageImageInfo` once the page is known.
        let center = self.residency.page_center(page_idx);
        let (view_sync, mut tabs, _) = self.view_sync_parts();
        view_sync.open_page(&mut tabs, tab, page_idx, center);
    }

    /// Applies one tab -> app request, right after the draw that raised it (`StudioApp::ui`), at
    /// the point the tab arms used to apply it. A tab switch here is seen by the frame's later
    /// steps (trims, hotkeys, `ViewSync::after_draw`) but not by the panel-dock verdict, which was
    /// taken for the tab that drew (`PanelDockPass`).
    fn apply_app_request(&mut self, request: AppRequest) {
        match request {
            AppRequest::PageOp(op) => self.start_page_op(op),
            AppRequest::OpenPage { tab, page_idx } => self.open_page_in_tab(tab, page_idx),
            AppRequest::NavigateSettings(link) => {
                self.tabs.settings.navigate_to(link);
                self.active_tab = AppTab::Settings;
            }
            AppRequest::CharactersChanged => {
                self.tabs.notes.notify_characters_changed();
                self.tabs.translation.state_mut().notify_characters_changed();
            }
            AppRequest::OpenNotesForCharacter(name) => {
                self.tabs.notes.notify_characters_changed();
                self.tabs.notes.set_character_context(name);
                self.set_active_tab(AppTab::Notes);
            }
            AppRequest::TermsChanged => self.tabs.notes.notify_terms_changed(),
        }
    }

    /// Splits the borrow of `self` into the view-sync state, the view tabs it drives and the
    /// project (read by the PS editor's flush and view sync).
    fn view_sync_parts(&mut self) -> (&mut ViewSync, ViewTabs<'_>, &ProjectData) {
        (
            &mut self.view_sync,
            self.tabs.view_tabs(),
            &self.project,
        )
    }

    /// Source pages the active tab needs resident (`StudioTab::source_page_window`): a canvas
    /// tab's visible pages plus `neighbor_radius`, the page manager viewer's one page, nothing for
    /// the others (the PS editor keeps its own residency through `CleanOverlaysModel`).
    fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.tabs.get(self.active_tab).source_page_window(neighbor_radius)
    }

    /// Whether the active tab draws source pages with NEAREST sampling (`StudioTab::wants_nearest_source`).
    fn active_canvas_source_pixel_inspection(&self) -> bool {
        self.tabs.get(self.active_tab).wants_nearest_source()
    }

    /// Per-frame source-texture trim: the active tab's page window and pixel-inspection state
    /// (tab knowledge, decided here) handed to [`PageResidency::trim_gpu`].
    fn trim_source_page_gpu_after_active_canvas(&mut self) {
        let budget = self.residency.memory_budget();
        let active_pages = self.active_source_page_window(budget.visible_neighbor_pages);
        let pixel_inspection = self.active_canvas_source_pixel_inspection();
        self.residency.trim_gpu(budget, active_pages, pixel_inspection);
    }

    fn trim_tab_gpu_after_active_canvas(&mut self) {
        let budget = self.residency.memory_budget();
        let active_pages = self.active_source_page_window(budget.visible_neighbor_pages);
        let pinned_pages = active_pages.into_iter().collect::<BTreeSet<_>>();
        let pressure = if budget.profile == crate::memory_manager::MemoryProfile::Minimal {
            MemoryPressure::Soft
        } else {
            MemoryPressure::Normal
        };
        let request = CacheEvictionRequest {
            profile: budget.profile,
            pressure,
            target_free_bytes: if pressure == MemoryPressure::Normal {
                0
            } else {
                u64::MAX
            },
            pinned_pages,
        };
        // Eviction only runs under non-Normal pressure (Minimal profile / Soft pressure).
        // `Tabs::evict_tab_gpu_caches` builds its own per-tab sub-snapshots internally, so no
        // separate snapshot is computed here.
        if request.pressure != MemoryPressure::Normal {
            let report = self.tabs.evict_tab_gpu_caches(&request);
            if !report.resources.is_empty() {
                runtime_log::log_info(format!(
                    "[memory] tab GPU trim freed {} resources, {} bytes",
                    report.resources.len(),
                    report.estimated_freed_bytes
                ));
            }
        }
    }

    /// Throttled memory-pressure response: when the residency's poll reports pressure, evicts the
    /// residency's caches (source GPU textures, then the shared page / overlay caches) and then
    /// the tabs' GPU caches, pinned to the active tab's page window, and logs the total.
    fn poll_memory_pressure_and_evict(&mut self) {
        let Some(tick) = self.residency.poll_pressure(Instant::now()) else {
            return;
        };
        let budget = self.residency.memory_budget();
        let pinned_pages = self
            .active_source_page_window(budget.visible_neighbor_pages)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let request = tick.eviction_request(pinned_pages);

        let mut report = self.residency.evict_for_pressure(&request, &self.clean_overlays_model);
        report.append(self.tabs.evict_tab_gpu_caches(&request));

        if report.estimated_freed_bytes > 0 {
            runtime_log::log_info(format!(
                "[memory] pressure={:?} evicted_resources={} estimated_freed_bytes={}",
                request.pressure,
                report.resources.len(),
                report.estimated_freed_bytes
            ));
        }
    }

    fn sync_project_from_bubbles_model(&mut self) {
        let Ok(model) = self.bubbles_model.lock() else {
            return;
        };
        let revision = model.revision();
        if revision == self.applied_bubbles_revision {
            return;
        }
        self.project.bubbles = model.snapshot_shared();
        self.applied_bubbles_revision = revision;
        self.session.note_unsaved();
    }

    fn draw_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for tab in AppTab::ALL {
                if !self.tab_visible(tab) {
                    continue;
                }
                let selected = self.active_tab == tab;
                if ui.selectable_label(selected, tab.title()).clicked() {
                    self.active_tab = tab;
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t!("app.menu.to_launcher_button")).clicked() {
                    if self.has_unsaved_changes() {
                        self.close.open_dialog(PendingCloseAction::ReturnToLauncher);
                    } else {
                        self.close.finalize(PendingCloseAction::ReturnToLauncher);
                    }
                }
                // The project's save button + status line, or the image set's save buttons.
                let (session, mut parts) = self.session_parts();
                session.draw_top_bar(ui, &mut parts);
                self.draw_storage_reconcile_notice(ui);
            });
        });
    }

    /// Top-bar notice of a startup storage-mode reconciliation that ended incomplete (a
    /// direct `--project` start runs it inside the studio, where the launcher's notice is
    /// never shown). Error-coloured, lists the failed files on hover (the shared list of the
    /// General pane), and stays until clicked. A user switch reports in the General pane
    /// itself, so only `JobOrigin::StartupReconciliation` is surfaced here. Per frame this
    /// reads the job slot only (no I/O).
    fn draw_storage_reconcile_notice(&mut self, ui: &mut egui::Ui) {
        use crate::storage_mode_job::{self as job, JobOrigin};
        if job::conversion_job_state().is_running() {
            // The worker finishes without an input event; poll for its end.
            ui.ctx().request_repaint_after(web_time::Duration::from_millis(500));
            return;
        }
        let Some(finished) = job::last_finished_job() else { return };
        if finished.origin != JobOrigin::StartupReconciliation || finished.outcome.is_complete() || self.storage_notice_dismissed_job == Some(finished.id) {
            return;
        }
        let text = if finished.outcome.mode_persisted {
            tf!("app.storage_mode.reconcile_failed", count = finished.outcome.failed.len())
        } else {
            t!("app.storage_mode.reconcile_not_switched").to_string()
        };
        let color = ui.visuals().error_fg_color;
        let response = ui
            .add(egui::Label::new(egui::RichText::new(text).color(color)).sense(egui::Sense::click()))
            .on_hover_ui(|ui| {
                ms_settings_ui::storage_mode_setting::draw_conversion_failures(ui, &finished.outcome);
                ui.small(t!("app.storage_mode.dismiss_hint"));
            });
        if response.clicked() {
            self.storage_notice_dismissed_job = Some(finished.id);
        }
    }
}

// The AI backend process + health/device probe outlive the studio window now (owned
// by `AiBackendSupervisor` in `startup::run_native`), so `StudioApp` no longer stops them on drop.

impl eframe::App for StudioApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // egui 0.35 replaced `App::update(&Context, …)` with `App::ui(&mut Ui, …)`: the framework
        // now hands us the window-root `Ui` that fills the whole window. The top-level panels below
        // (`Panel::top`, `CentralPanel`) build on this `ui`; floating windows/prompts and every
        // context-level call keep using a borrowed `Context` handle cloned from it. Cloning (Arc
        // inside) is required so the context handle does not keep `ui` borrowed while the root
        // panels mutably borrow it.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Advance the puffin frame and open the top-level scope for this frame. `new_frame`
        // must run exactly once per frame before any `profile_scope!` so recorded scopes are
        // attributed to the correct frame; the scope guard stays alive for the whole body.
        #[cfg(feature = "profiling")]
        puffin::GlobalProfiler::lock().new_frame();
        #[cfg(feature = "profiling")]
        puffin::profile_scope!("StudioApp::update");
        if let Ok(mut doc) = self.layer_doc.lock() {
            doc.poll_save_acks();
        }

        let now = ctx.input(|i| i.time);
        // The session step, every frame whatever tab is active (`SessionMode::tick`). Project: the
        // deferred-save preload, the save / page-op / discard-cleanup polls, the unsaved cache, the
        // status expiry and the save-then-close, in that order. Image set: the save driver (also the
        // one `poll_flatten_to_file` caller the typing tab requires) and its close outcome.
        let (session, mut parts) = self.session_parts();
        session.tick(ctx, now, &mut parts);
        // A save-to-project merge that succeeded in this tick changed the committed chapter tree;
        // the tabs caching a view of it reschedule their scans now, before any of them draws.
        if self.session.take_committed_tree_changed() {
            self.tabs.for_each_mut(|tab| tab.on_committed_tree_changed());
        }

        {
            // Coarse marker for the per-frame worker polling + GPU upload phase: decoded page
            // promotion, incremental texture upload, and overlay/page-cache loader draining.
            #[cfg(feature = "profiling")]
            puffin::profile_scope!("poll_loaders_and_upload");
            self.residency.poll_decoded(ctx, &self.clean_overlays_model);
            // Every tab's own background poll, whatever tab is active, before any gate reads
            // `clean_op_in_flight` (page ops, save-to-project): the page manager drains its clean
            // worker and frees the hidden viewer's clean tiles here (`StudioTab::poll_background`).
            let project = &self.project;
            self.tabs.for_each_mut(|tab| tab.poll_background(ctx, project));
            self.residency.poll_page_cache(&self.project, &self.clean_overlays_model);
        }
        self.sync_project_from_bubbles_model();
        // The input router, ONCE per frame: after the session tick (a save it finished may have
        // closed a dialog) and BEFORE the top bar and the tab draw, so every consumer of this
        // frame reads one `InputFrame` and the keys it matched are consumed before any widget
        // sees them. Contexts come from the active tab; the two modal prompts suppress input.
        // The AI device prompt is opened (not drawn) here, from this frame's health snapshot, so
        // its modal state is known before the router runs; it is drawn after the tab as before.
        self.refresh_ai_backend_health_cache();
        self.ai_device_prompt.refresh(&self.ai_backend_health_cached, self.ai_backend_probe_tx.as_ref());
        // The one owner of "a binding capture never outlives the Settings tab": a capture
        // suppresses every binding, so it ends as soon as another tab is active.
        cancel_capture_outside_settings(&mut self.input, self.active_tab);
        let input_env = FrameEnv {
            active_tab: Some(self.active_tab),
            contexts: self.tabs.get(self.active_tab).input_contexts(),
            modal_open: self.comic_type_prompt.is_open() || self.ai_device_prompt.is_open_for(&self.ai_backend_health_cached),
        };
        let input_frame = self.input.begin_frame(ctx, &input_env);
        // The session's own commands (the image set's save / save-as); tab commands are read by
        // the drawing tab from the same frame.
        self.session.run_fired_commands(ctx, input_frame);
        if self.residency.background_loads_pending() {
            ctx.request_repaint();
        }

        egui::Panel::top("top_bar").show(ui, |ui| {
            self.draw_tab_bar(ui);
        });

        // Show exit/leave dialog on top of all other content.
        self.draw_exit_dialog(ctx);
        self.draw_incoming_confirm(ctx);
        // The image set's JPEG options / folder / skipped-files dialogs, or the project's page-op
        // veil and failure dialog.
        let (session, mut parts) = self.session_parts();
        session.draw_dialogs(ctx, &mut parts);

        let view_tab = self.active_tab;
        let (view_sync, mut tabs, project) = self.view_sync_parts();
        view_sync.before_draw(&mut tabs, view_tab, project);

        // Coarse marker for the active-tab draw, the single most expensive phase per frame
        // (canvas/overlay rendering, side panels, tab-specific UI). `profile_scope_custom!`
        // returns the guard so it can be held across the draw and the requests it raised; it is
        // dropped after the idle sub-window pass. `_active_tab_draw_scope` is a held RAII guard, not dead code.
        #[cfg(feature = "profiling")]
        let _active_tab_draw_scope = puffin::profile_scope_custom!("active_tab_draw");
        // The panel-dock verdict of this pass, taken BEFORE the tab draws and used afterwards: a
        // page-manager open, the typing tab's font-group deep link into the settings tab and the
        // characters tab's «open notes» switch `self.active_tab` (as `AppRequest`s applied after
        // the draw), so a verdict read after them answers for the tab the frame ENDED on, not for
        // the one that drew.
        let drawn_tab = self.active_tab;
        let dock_pass = PanelDockPass::begin(self.tabs.hosts_panel_dock(drawn_tab));
        // The frame context of the drawing tab. The busy snapshot is taken before the tab states
        // are borrowed (`busy.rs`); the rest are reads of the residency and the lent loan.
        let activity = self.persistence_activity();
        let status = CanvasUiStatus {
            loaded_pages: self.residency.loaded_pages_count(),
            total_pages: self.project.pages.len(),
            load_errors_count: self.residency.load_errors().len(),
        };
        let overlays_loading = self.residency.overlays_loading();
        // Lent for the frame: every tab that draws pages (the canvases, the page manager's viewer)
        // draws from the resident tiles instead of decoding them again.
        let (page_infos, textures) = self.residency.lend_mut();
        let requests = self.tabs.draw(
            drawn_tab,
            ui,
            TabFrame {
                ctx,
                project: &self.project,
                page_infos,
                textures,
                status,
                overlays_loading,
                activity,
                panel_dock: &mut self.panel_dock,
                input: &mut self.input,
            },
        );
        // Panel-layout persistence poll, right after a dock host drew: the dock raises its dirty
        // flag DURING a gesture — on every frame a panel drag advances — so the flag must be picked
        // up in the same frame it was raised; the writer is what turns that per-frame stream into
        // one debounced write. Before the requests, as it ran before the typing deep link.
        if dock_pass.dock_ran()
            && let Some(layouts) = self.panel_dock.take_dirty_layouts()
        {
            self.panel_layout_writer.store(layouts);
        }
        // What the tab asked of the app, in the order raised; the tab switches among them land here.
        for request in requests {
            self.apply_app_request(request);
        }
        // The dock's detached panel windows are immediate viewports: they exist only
        // while they are shown every pass, and `PanelDock::end` — which shows them —
        // ran above only if the tab that DREW this pass hosts the dock. Requirement 11
        // of `dev-docs/dockable_panels_plan.md` says they stay open and empty
        // otherwise, so they are shown here — and ONLY here, because showing one
        // viewport twice in a pass would render it twice. The verdict comes from
        // `dock_pass`, decided before the tab drew: `self.active_tab` may name another
        // tab by now, and either mistake is visible (a window shown zero times is
        // dropped by eframe and jumps when it is re-created a frame later; a window
        // shown twice renders grey and consumes its input twice).
        if dock_pass.needs_idle_sub_windows() {
            self.panel_dock.show_idle_sub_windows(ctx);
        }
        // Close the active-tab draw scope before the post-draw GPU trim / memory phases.
        #[cfg(feature = "profiling")]
        drop(_active_tab_draw_scope);
        self.trim_source_page_gpu_after_active_canvas();
        self.trim_tab_gpu_after_active_canvas();
        self.poll_memory_pressure_and_evict();

        if let Some(comic_type) = self.comic_type_prompt.draw(ctx) {
            self.apply_comic_type_preset(comic_type);
        }
        // Drawn with the snapshot `FrameEnv` was decided with (taken before the router ran).
        let ai_device_prompt_open = self.ai_device_prompt.draw(ctx, &self.ai_backend_health_cached, self.ai_backend_probe_tx.as_ref());
        // While a modal prompt is open the router suppresses input; keep repainting so the
        // prompt stays responsive.
        if self.comic_type_prompt.is_open() || ai_device_prompt_open {
            ctx.request_repaint();
        }
        // The tab the frame ENDED on (a page-manager open or a deep link may have switched it).
        let view_tab = self.active_tab;
        let (view_sync, tabs, _) = self.view_sync_parts();
        view_sync.after_draw(&tabs, view_tab);

        // Frame profiler window (feature-gated). F11 toggles visibility; `profiler_window`
        // returns false when the user closes the window, which we mirror into the field.
        #[cfg(feature = "profiling")]
        {
            if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
                self.profiler_window_open = !self.profiler_window_open;
            }
            if self.profiler_window_open {
                self.profiler_window_open = puffin_egui::profiler_window(ctx);
                // The profiler view updates every frame; keep repainting while it is open.
                ctx.request_repaint();
            }
        }
    }
}

/// What one `StudioApp::ui` pass did with the panel dock, decided BEFORE the active tab draws.
///
/// The dock's detached sub-windows are immediate viewports and must be shown exactly once per
/// pass: `PanelDock::end` shows them while a dock host draws, and `show_idle_sub_windows` covers
/// every other pass. Which of the two applies is a fact about the tab that DREW, and that tab is
/// not `StudioApp::active_tab` at the end of the pass — the page manager's «open in tab» and the
/// typing tab's settings deep link both switch tabs right after the draw (`AppRequest::OpenPage`,
/// `AppRequest::NavigateSettings`), before the idle pass runs. Capturing the verdict up
/// front is what keeps the two mistakes out: a pass that shows a viewport zero times has it
/// dropped by eframe and re-created without its stored position (the window jumps), and a pass
/// that shows one twice renders it empty and feeds it its input twice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct PanelDockPass {
    /// `true` when the tab that draws this pass runs `PanelDock::end`, which shows every
    /// sub-window viewport of the dock itself.
    dock_ran: bool,
}

impl PanelDockPass {
    /// Opens the pass for the tab that is about to draw; `drawn_tab_hosts_dock` is that tab's
    /// answer to "is a dock host" (`Tabs::hosts_panel_dock`). Call before `Tabs::draw`.
    #[must_use]
    fn begin(drawn_tab_hosts_dock: bool) -> Self {
        Self {
            dock_ran: drawn_tab_hosts_dock,
        }
    }

    /// Whether the tab that drew this pass ran the dock (and may have raised its dirty flag).
    #[must_use]
    fn dock_ran(self) -> bool {
        self.dock_ran
    }

    /// Whether `PanelDockState::show_idle_sub_windows` must run after the draw, i.e. whether the
    /// pass has not already shown the sub-window viewports.
    #[must_use]
    fn needs_idle_sub_windows(self) -> bool {
        !self.dock_ran
    }
}

/// Builds this studio window's panel-dock state and restores the persisted arrangement — and the
/// extra per-tab state stored beside it — from an already loaded `user_config.json` snapshot.
/// Performs no I/O.
///
/// Call once, before the first frame: a restored layout wins over a program tab's default layout,
/// while an absent, malformed, newer or invalid stored layout silently degrades to it (every reason
/// is logged by `panel_dock::persist`). Tabs the stored layout does not mention are re-created by
/// the dock on the frame that declares them.
///
/// `defaults` are the dock hosts' default-layout builders keyed by `AppTab::key()`
/// (`Tabs::dock_layout_defaults`, from each tab's `StudioTab::default_dock_layout`): a tab starts
/// hosting the dock by answering that hook, nowhere else.
#[must_use]
fn restore_panel_dock(user_settings: &Value, defaults: &[panel_dock_persist::LayoutDefault<'static>]) -> PanelDockState {
    let restored = panel_dock_persist::layouts_from_user_settings(user_settings, defaults);
    let mut state = PanelDockState::new();
    // Layouts first: the sub-window install drops a window no restored layout puts a panel in, so
    // it has to see them.
    state.install_persisted_layouts(restored.layouts);
    state.install_persisted_sub_windows(restored.sub_windows);
    // Extra per-tab state (what a tab body stored next to the arrangement) is
    // independent of both: it belongs to the tab, not to the panel holding it.
    state.install_persisted_tab_extras(restored.tab_extras);
    state
}

/// Builds the studio's input registry: the shared view set (`ms_input::view::declarations`), the
/// canvas set (`ms_canvas::input::declarations`), the shared brush set
/// (`ms_tools::mask_brush::declarations`), then the tabs' `tab_declarations` (`Tabs::input_declarations`, `AppTab::ALL` order), then the session's
/// own `session_declarations` (`SessionMode::input_declarations`: the image set's save commands),
/// then the pre-read user `overrides` (`StudioBootstrap::hotkey_overrides`; an override that
/// does not apply is skipped and logged by the registry, the action keeps its defaults). No I/O.
///
/// # Errors
/// [`DeclarationError`] when the merged declarations are invalid (a duplicate id, a dangling mode
/// reference): a programming error the golden test `input_registry_golden` pins.
fn build_input_registry(tab_declarations: Declarations, session_declarations: Declarations, overrides: &HotkeyOverrides) -> Result<InputRegistry, DeclarationError> {
    let mut declarations = view::declarations();
    declarations.extend(ms_canvas::input::declarations());
    declarations.extend(ms_tools::mask_brush::declarations());
    declarations.extend(tab_declarations);
    declarations.extend(session_declarations);
    let mut registry = InputRegistry::new(declarations)?;
    let skipped = registry.apply_overrides(overrides);
    if !skipped.is_empty() {
        runtime_log::log_warn(format!("[app] {} of {} stored input binding override(s) were skipped; those actions keep their defaults", skipped.len(), overrides.actions.len()));
    }
    Ok(registry)
}

/// Ends a running binding capture when `active_tab` is not the Settings tab (the only tab that
/// draws the capture UI). Called once per frame right before `InputRouter::begin_frame`, so a
/// capture started in Settings › Hotkeys ends on the first frame another tab is active and can
/// never keep the studio's bindings suppressed. Returns whether a capture was cancelled.
fn cancel_capture_outside_settings(input: &mut InputRouter, active_tab: AppTab) -> bool {
    if active_tab == AppTab::Settings {
        return false;
    }
    let Some(action) = input.capture_status().map(|status| status.action) else {
        return false;
    };
    input.capture_cancel();
    runtime_log::log_info(format!("[app] binding capture of {} cancelled: the Settings tab is no longer active ({active_tab:?} is)", action.0));
    true
}

/// The registry `built`, or, when the declarations were invalid, an EMPTY registry: the session
/// still opens (every binding inert) instead of failing the whole studio, and the error is logged.
fn input_registry_or_empty(built: Result<InputRegistry, DeclarationError>) -> InputRegistry {
    match built {
        Ok(registry) => registry,
        Err(err) => {
            runtime_log::log_error(format!("[app] the studio's input declarations are invalid ({err}); no binding works in this session"));
            InputRegistry::empty()
        }
    }
}

/// Spawns a one-shot background thread that warms the process-global text-render FontSystem
/// pool, at most once per process.
///
/// The first `FontSystem` lease scans the system font directories (~2.2s in a fresh process,
/// ~32ms afterwards). Running that scan here, on a detached background thread, moves it off the
/// GUI thread and off the first text render's critical path. Idempotent across repeated
/// `StudioApp::new` calls (launcher<->studio transitions) via a `Once` guard, so no redundant
/// prewarm threads are spawned.
fn spawn_text_render_font_pool_prewarm() {
    static PREWARM_ONCE: std::sync::Once = std::sync::Once::new();
    PREWARM_ONCE.call_once(|| {
        runtime_log::log_info("[app] prewarming text-render font system pool in background");
        // Detached: the pool is process-global, the work is idempotent, and nothing on the GUI
        // thread waits on it. We must never join this on the GUI thread.
        thread::spawn(crate::tabs::typing::render_next::prewarm_font_system_pool);
    });
}

#[cfg(test)]
mod tests {
    use super::{PanelDockPass, build_input_registry, cancel_capture_outside_settings, input_registry_or_empty};
    use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
    use crate::tabs::AppTab;
    use crate::tabs::cleaning::CleaningTabState;
    use crate::tabs::translation::{TranslationTab, TranslationTabState};
    use crate::tabs::typing::TypingTabState;
    use eframe::egui;
    use ms_input::{ActionKind, Declarations, DeclarationError, HotkeyOverrides, InputRegistry, Scope, view};
    use ms_canvas::input::EDITABLE;
    use ms_studio_tab::StudioTab;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// The idle sub-window pass must answer for the tab that DREW, and the two requests that
    /// reassign `active_tab` right after the draw are exactly what makes the two answers differ:
    /// «Менеджер страниц» opens a page in a canvas tab, and «Текст» deep-links into the
    /// settings tab. Reading the tab AFTER the requests therefore either skips the keep-alive
    /// (eframe drops the viewport and the window jumps when it is re-created) or runs it on
    /// top of `PanelDock::end` (one viewport shown twice in one pass).
    #[test]
    fn the_idle_sub_window_pass_answers_for_the_tab_that_drew() {
        // Not a dock host (the page manager) -> the keep-alive is required, whatever the pass
        // ends on, and no dirty layout is polled.
        let pass = PanelDockPass::begin(false);
        assert!(
            pass.needs_idle_sub_windows(),
            "the page-manager pass drew no dock, so the windows still need showing"
        );
        assert!(!pass.dock_ran());

        // A dock host (typing) -> `PanelDock::end` already showed the viewports; showing them
        // again in the same pass renders each of them twice.
        let pass = PanelDockPass::begin(true);
        assert!(!pass.needs_idle_sub_windows());
        assert!(pass.dock_ran());
    }

    /// The tab-declared part of the studio's input: the only three tabs that declare or publish
    /// anything input-related, Translation, Cleaning and Typing (the other seven answer the trait's
    /// empty defaults). The bubbles model's documents live under a unique temp path that is never
    /// created: the model writes nothing unless it is edited.
    fn input_tabs() -> (TranslationTab, CleaningTabState, TypingTabState) {
        let (translation, cleaning) = translation_and_cleaning();
        (translation, cleaning, TypingTabState::default())
    }

    /// Translation and Cleaning only (see `input_tabs`).
    fn translation_and_cleaning() -> (TranslationTab, CleaningTabState) {
        let model_dir = std::env::temp_dir().join(format!("ms_input_golden_{}", std::process::id()));
        let model = BubblesModel::new_with_staging_probe(Vec::new(), model_dir.join("bubbles.json"), model_dir.join("unsaved_bubbles.json"), SharedCanvasSettings::default(), None, false);
        let translation = TranslationTab::new(TranslationTabState::default(), Arc::new(Mutex::new(model)), &json!({}));
        (translation, CleaningTabState::default())
    }

    /// Every tab's declarations merged in `AppTab::ALL` order, as `Tabs::input_declarations`
    /// merges them (the tabs not listed in `input_tabs` declare nothing).
    fn all_tab_declarations() -> Declarations {
        let (translation, cleaning, typing) = input_tabs();
        let mut declarations = Declarations::default();
        for tab in [&translation as &dyn StudioTab, &cleaning, &typing] {
            declarations.extend(tab.input_declarations());
        }
        declarations
    }

    /// Pins the part of the input registry the ROOT owns or composes directly (no tab actions, no
    /// user overrides): the view set as the root gates it, the canvas set, the brush set, the image
    /// set's session commands and the fixed rows — every action's id, kind, scope, fixed flag and
    /// default label in English, in registry order. Tab actions are pinned once, by each tab
    /// crate's own golden (`ms-tab-translation` `input/tests.rs::declarations_golden`, and the
    /// Cleaning and Typing goldens), never here. Ids are persistence keys in `user_config` ->
    /// `Hotkeys`; the format-1 ids they replace migrate in `ms_input::persist`.
    ///
    /// Also checks the FULL registry, built exactly as `StudioApp::new` builds it (view + canvas +
    /// brush + every tab + the image-set session): it must be valid and its defaults conflict-free.
    /// The registry `built` returned, or a test failure naming `what`.
    fn registry_or_panic(built: Result<InputRegistry, DeclarationError>, what: &str) -> InputRegistry {
        match built {
            Ok(registry) => registry,
            Err(err) => panic!("{what} must be valid: {err}"),
        }
    }

    /// Installs the embedded English catalog for a label assertion; the caller holds the global
    /// locale lock (`english_locale`) for the whole test.
    fn english_locale() -> std::sync::MutexGuard<'static, ()> {
        let guard = match crate::locale_store::GLOBAL_LOCALE_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(err) = ms_i18n::set_locale(&ms_i18n::LocaleTag::english()) {
            panic!("the embedded English catalog must install: {err}");
        }
        guard
    }

    #[test]
    fn input_registry_golden() {
        use ActionKind::{Axis, Command, Gesture, Mode};
        const CV: Scope = Scope::Canvas;
        const BRUSH: Scope = Scope::Sub(None);
        // Held for the whole test: the active catalog is process-global.
        let _locale = english_locale();

        let full = registry_or_panic(build_input_registry(all_tab_declarations(), crate::studio::session::image_set::input_declarations(), &HotkeyOverrides::default()), "the full studio declarations");
        assert!(full.conflicts().conflicts.is_empty(), "the default bindings of the full studio registry conflict: {:#?}", full.conflicts().conflicts);

        let registry = registry_or_panic(build_input_registry(Declarations::default(), crate::studio::session::image_set::input_declarations(), &HotkeyOverrides::default()), "the root declarations");
        let ctrl = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };
        let zoom = format!("({ctrl} / Z)");
        let minus = egui::Key::Minus.symbol_or_name();
        let lmb_drag = "LMB drag".to_owned();
        // The platform's clipboard chords (`egui-winit` adds the Insert / Delete spellings on
        // Windows only).
        let clipboard = |key: &str, windows: &str| if cfg!(target_os = "windows") { format!("{ctrl}+{key} / {windows}") } else { format!("{ctrl}+{key}") };
        let expected: Vec<(&str, ActionKind, Scope, bool, String)> = vec![
            ("view.zoom_modifier", Mode, Scope::View, false, format!("{ctrl} / Z")),
            ("view.pan", Mode, Scope::View, false, "Space".to_owned()),
            ("view.zoom_in", Command, Scope::View, false, format!("{zoom}+= / {zoom}+Shift+= / {zoom}++")),
            ("view.zoom_out", Command, Scope::View, false, format!("{zoom}+{minus}")),
            ("view.zoom_reset", Command, Scope::View, false, format!("{zoom}+0")),
            ("view.zoom", Axis, Scope::View, false, format!("{zoom}+Wheel / Pinch")),
            ("view.zoom_drag", Gesture, Scope::View, false, format!("{zoom}+LMB drag")),
            ("canvas.bubble.undo", Command, CV, false, format!("{ctrl}+Z")),
            ("canvas.bubble.redo", Command, CV, false, format!("{ctrl}+Shift+Z / {ctrl}+Y")),
            ("canvas.bubble.duplicate", Command, CV, false, format!("{ctrl}+D")),
            ("canvas.bubble.select", Gesture, CV, false, "LMB click".to_owned()),
            ("canvas.bubble.menu", Gesture, CV, false, "RMB click".to_owned()),
            ("canvas.bubble.drag", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.bubble.move", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.bubble.move_rect", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.bubble.resize", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.image_area.move_rect", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.image_area.move", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.image_area.move_anchor", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.image_area.resize", Gesture, CV, false, lmb_drag.clone()),
            ("canvas.page.click", Gesture, CV, false, "LMB click".to_owned()),
            ("canvas.page.menu", Gesture, CV, false, "RMB click".to_owned()),
            ("brush.size", Axis, BRUSH, false, "Shift+Wheel / Shift+Horizontal wheel".to_owned()),
            ("brush.smaller", Command, BRUSH, false, minus.to_owned()),
            ("brush.larger", Command, BRUSH, false, "= / Shift+= / +".to_owned()),
            ("single_image.save", Command, Scope::Session, false, format!("{ctrl}+S")),
            ("single_image.save_as", Command, Scope::Session, false, format!("{ctrl}+Shift+S")),
            ("view.scroll", Axis, Scope::View, true, "Wheel".to_owned()),
            ("view.scroll_horizontal", Axis, Scope::View, true, "Shift+Wheel / Horizontal wheel".to_owned()),
            ("canvas.bubble.copy", Command, CV, true, clipboard("C", &format!("{ctrl}+Ins"))),
            ("canvas.bubble.cut", Command, CV, true, clipboard("X", "Shift+Del")),
            ("canvas.bubble.paste", Command, CV, true, clipboard("V", "Shift+Ins")),
        ];
        let actual: Vec<(&str, ActionKind, Scope, bool, String)> = registry.actions().map(|action| (action.meta.id.0, action.kind, action.meta.scope, action.fixed, registry.label(action.meta.id))).collect();
        assert_eq!(actual, expected);
    }

    /// The view set is registered as `ms_input::view` declares it (no root gate: every viewer acts
    /// only on input focused on its own view), and the canvas tabs forward their canvas's
    /// contexts: Translation edits bubbles, Cleaning and Typing do not.
    #[test]
    fn the_view_set_is_ungated_and_canvas_contexts_are_forwarded() {
        let registry = registry_or_panic(build_input_registry(Declarations::default(), Declarations::default(), &HotkeyOverrides::default()), "the root declarations");
        for action in view::declarations().actions {
            let id = action.meta().id;
            assert_eq!(registry.spec(id), Some(&action), "{id:?} must stay as `ms_input::view` declares it");
        }
        let (translation, cleaning, typing) = input_tabs();
        assert!(translation.input_contexts().contains(&EDITABLE));
        assert!(!cleaning.input_contexts().contains(&EDITABLE));
        assert!(!typing.input_contexts().contains(&EDITABLE));
    }

    /// The image set's two save commands join the registry without a conflict (exact modifier sets:
    /// Ctrl+S is neither Ctrl+Shift+S nor the Translation panel's plain S), and a stored override
    /// replaces a default.
    #[test]
    fn session_commands_and_overrides_apply() {
        let (translation, _) = translation_and_cleaning();
        let mut overrides = HotkeyOverrides::default();
        overrides.actions.insert("translation.panel.ocr.toggle".to_owned(), Vec::new());
        let registry = registry_or_panic(build_input_registry(translation.input_declarations(), crate::studio::session::image_set::input_declarations(), &overrides), "the Translation and session declarations");
        assert!(registry.action("single_image.save").is_some() && registry.action("single_image.save_as").is_some());
        assert!(registry.conflicts().conflicts.is_empty(), "{:?}", registry.conflicts().conflicts);
        assert!(registry.bindings(ms_input::ActionId("translation.panel.ocr.toggle")).is_empty(), "the override unbinds the action");
    }

    /// Invalid declarations (a duplicate id) never fail the session: the studio gets an empty,
    /// inert registry instead.
    #[test]
    fn invalid_declarations_fall_back_to_an_empty_registry() {
        let built = build_input_registry(view::declarations(), Declarations::default(), &HotkeyOverrides::default());
        assert!(built.is_err(), "the view set declared twice must be rejected");
        let registry: InputRegistry = input_registry_or_empty(built);
        assert_eq!(registry.actions().count(), 0);
    }

    /// Runs one headless frame through `router.begin_frame` and returns whether input was
    /// suppressed in it.
    fn frame_suppressed(ctx: &egui::Context, router: &mut ms_input::InputRouter, active_tab: AppTab) -> bool {
        let env = ms_input::FrameEnv { active_tab: Some(active_tab), contexts: Vec::new(), modal_open: false };
        let mut suppressed = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            suppressed = Some(router.begin_frame(ui.ctx(), &env).suppressed());
        })
        .drop_without_applying_deltas();
        match suppressed {
            Some(suppressed) => suppressed,
            None => panic!("run_ui must call the frame closure"),
        }
    }

    /// A capture started in Settings › Hotkeys keeps suppressing input only while the Settings
    /// tab is active: the first frame another tab is active ends it before the router runs, and
    /// with no capture the helper does nothing.
    #[test]
    fn a_binding_capture_ends_when_the_settings_tab_is_left() {
        let registry = registry_or_panic(build_input_registry(Declarations::default(), Declarations::default(), &HotkeyOverrides::default()), "the root declarations");
        let mut router = ms_input::InputRouter::new(registry);
        let ctx = egui::Context::default();
        router.capture_begin(view::ZOOM_IN, ms_input::CaptureSlot::Append);

        assert!(!cancel_capture_outside_settings(&mut router, AppTab::Settings), "Settings keeps its capture");
        assert!(frame_suppressed(&ctx, &mut router, AppTab::Settings), "a running capture suppresses input");

        // The user switched to Translation: the root's per-frame step before `begin_frame`.
        assert!(cancel_capture_outside_settings(&mut router, AppTab::Translation));
        assert!(router.capture_status().is_none());
        assert!(!frame_suppressed(&ctx, &mut router, AppTab::Translation), "bindings work again on the first frame away from Settings");
        assert!(!cancel_capture_outside_settings(&mut router, AppTab::Translation), "nothing to cancel any more");
    }
}
