/*
FILE OVERVIEW: src/studio.rs
Main eframe application orchestration (`StudioApp`): project/model initialization,
background page/overlay loading, tab routing, and global hotkey dispatch.

Main structs:
- `StudioApp`: root app state (project, shared models, tab states, residency, hotkeys).
The source-page residency pipeline (decode pool, in-order promotion, budgeted tiled GPU upload,
overlay and page-cache loaders, memory-pressure eviction) is `PageResidency` in `studio/residency/`;
this file only feeds it the tab knowledge it must not have: the active tab's page window and
pixel-inspection state (`active_source_page_window`, `active_canvas_source_pixel_inspection`),
the canvas `cache_pages` setting, and the tabs' own GPU caches (`evict_tab_gpu_caches`).

App/frame flow:
- `new(project, bootstrap, ai_backend)`: builds models, tabs, and input manager registrations
  from the inputs `StudioBootstrap` (`studio/bootstrap.rs`) pre-read on the open worker; it reads
  no config or document itself, so it can run inside a GUI frame.
- `new`: wires shared models (`bubbles`, `clean_overlays`, `text_mask`) between tabs.
- `new`: also starts shared AI-backend health probe and wires it into Settings/Translation tabs.
- `ui` (eframe::App, egui 0.35): frame tick (poll workers, draw active tab, dispatch hotkeys);
  lends `PageResidency::lend_mut` (page geometry + resident textures) to the drawing tab.
- Tabs: the ten tab states and the Translation canvas are `Tabs` (`studio/tabs.rs`). `ui` builds the
  frame context (`TabFrame`), calls `Tabs::draw` for the active tab, polls the panel dock's dirty
  layouts when that tab hosts the dock, then applies the returned `AppRequest`s
  (`apply_app_request`: page op, page open, settings deep link, characters / terms changed).
- `dispatch_hotkeys` + `execute_hotkey_command`: execute `InputManagerV2` commands.
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

Hotkeys:
- Translation canvas zoom/edit commands + panel toggles (`P/O/K/M/D` by default).
- Cleaning canvas zoom commands.
- Image-set save / save-as (registered only in that mode, `SessionMode::hotkey_specs`).

Profiling (optional, behind the `profiling` cargo feature):
- `ui` is instrumented with coarse `puffin::profile_scope!` markers around the heavy
  per-frame phases (worker polling, GPU upload, active-tab draw). The puffin_egui profiler
  window is shown while the feature is compiled in. All puffin references are gated behind
  `#[cfg(feature = "profiling")]` so the default build pulls in no puffin and has zero overhead.
  How to run: `cargo run --features profiling`.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::canvas::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleMode, BubbleTextField, BubbleType,
    CanvasUiStatus, CanvasView, OnTopFocusMode, TranslationStatusDisplay,
};
use crate::input_manager_v2::{HotkeyOverrides, HotkeyScopeV2, HotkeySpecV2, InputManagerV2};
use serde_json::{Map, Value};
use crate::memory_manager::{CacheEvictionReport, CacheEvictionRequest, MemoryManager, MemoryPressure};
use crate::models::autosave_gate::AutosaveGate;
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::text_mask_model::TextMaskModel;
use crate::project::{ComicType, ProjectData, save_comic_type_to_project_file};
use crate::page_ops::PageOpKind;
use crate::runtime_log;
use crate::tabs::AppTab;
use crate::tabs::characters::CharactersTabState;
use crate::tabs::cleaning::{CleaningTabState, cleaning_default_dock_layout};
use crate::tabs::notes::NotesTabState;
use crate::tabs::page_manager::PageManagerTabState;
use crate::tabs::ps_editor::{PsEditorTabState, ps_editor_default_dock_layout};
use crate::tabs::settings::SettingsTabState;
use crate::tabs::terms::TermsTabState;
use crate::tabs::translation::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand};
use crate::tabs::translation::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints, TranslationTabState,
    translation_default_dock_layout,
};
use crate::tabs::typing::{TypingTabState, typing_default_dock_layout};
use crate::tabs::wiki::WikiTabState;
use crate::widgets::panel_dock::persist as panel_dock_persist;
use crate::widgets::panel_dock::{DockLayout, PanelDockState, PanelLayoutWriter};
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
use residency::{PageResidency, append_eviction_report};
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
use session::{SessionMode, SessionParts, image_set};
// Named by the main window's Closing screen (`src/shell/closing.rs`), which runs the teardown.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use shutdown::ShutdownReport;

const HOTKEY_TRANSLATION_ZOOM_IN: &str = "translation.canvas.zoom_in";
const HOTKEY_TRANSLATION_ZOOM_OUT: &str = "translation.canvas.zoom_out";
const HOTKEY_TRANSLATION_ZOOM_RESET: &str = "translation.canvas.zoom_reset";
const HOTKEY_TRANSLATION_DELETE_BUBBLE: &str = "translation.bubble.delete_selected";
const HOTKEY_TRANSLATION_CREATE_BUBBLE: &str = "translation.bubble.create_at_cursor";
const HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE: &str = "translation.bubble.create_hint_at_cursor";
const HOTKEY_CLEANING_ZOOM_IN: &str = "cleaning.canvas.zoom_in";
const HOTKEY_CLEANING_ZOOM_OUT: &str = "cleaning.canvas.zoom_out";
const HOTKEY_CLEANING_ZOOM_RESET: &str = "cleaning.canvas.zoom_reset";
/// `user_config.json` section holding the per-tab canvas bottom-hint collapsed flags.
const CANVAS_CONFIG_SECTION: &str = "Canvas";
/// Config keys (under [`CANVAS_CONFIG_SECTION`]) for each canvas tab's bottom-hint collapsed state.
/// Default (absent) is `false` = expanded. Written only on exit; seeded once at construction. Only
/// Translation and Typing have a bottom-hint; Cleaning's hint is disabled and has no key. A stale
/// `cleaning_hint_collapsed` may linger in an old config; it is never read or written.
const CANVAS_TRANSLATION_HINT_COLLAPSED_KEY: &str = "translation_hint_collapsed";
const CANVAS_TYPING_HINT_COLLAPSED_KEY: &str = "typing_hint_collapsed";
/// `user_config.json` section holding typing-tab settings (persisted centering-assist flags, etc.).
const TEXT_TAB_CONFIG_SECTION: &str = "TextTab";
/// Config keys (under [`TEXT_TAB_CONFIG_SECTION`]) for the two persisted centering-assist checkboxes.
/// `centering_assist_enabled` defaults (absent) to `false` (off); `centering_show_center` defaults to
/// `true` (marker shown). Written only on exit; seeded once at construction. The bound-center KIND is
/// session-only and has no key. The dependent bound-center marker's default-on state is why its key
/// uses a `true` default while the assist toggle uses `false`.
const TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY: &str = "centering_assist_enabled";
const TEXT_TAB_CENTERING_SHOW_CENTER_KEY: &str = "centering_show_center";

#[derive(Debug, Clone, Default)]
struct CachedHotkeyHints {
    bindings_revision: u64,
    create_bubble_hint: Option<String>,
    translation_hints: TranslationHotkeyHints,
}

pub struct StudioApp {
    project: ProjectData,
    /// Source-page residency: decode pool, in-order promotion, budgeted GPU upload, overlay and
    /// page-cache loaders, page geometry + resident textures lent to the drawing tab, and the
    /// memory-pressure eviction of what it owns (`studio/residency/`).
    residency: PageResidency,
    bubbles_model: Arc<Mutex<BubblesModel>>,
    clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    applied_bubbles_revision: u64,
    cached_hotkey_hints: CachedHotkeyHints,
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
    /// The ten program-tab states and the Translation canvas (`studio/tabs.rs`); the active one draws
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
    input_manager_v2: InputManagerV2,
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
        // Per-tab canvas bottom-hint collapsed state, seeded once from `user_config` (absent =>
        // expanded). Read back on exit and persisted via `shutdown_blocking`; not saved on every toggle.
        let translation_hint_collapsed =
            hint_collapsed_from_user_settings(&user_settings, CANVAS_TRANSLATION_HINT_COLLAPSED_KEY);
        let typing_hint_collapsed =
            hint_collapsed_from_user_settings(&user_settings, CANVAS_TYPING_HINT_COLLAPSED_KEY);
        // Persisted centering-assist checkboxes, seeded once from `user_config` (absent => assist off,
        // marker shown). Read back on exit and persisted via `shutdown_blocking`; not saved on every toggle. The
        // bound-center KIND stays session-only.
        let typing_centering_assist_enabled = text_tab_bool_from_user_settings(
            &user_settings,
            TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY,
            false,
        );
        let typing_centering_show_center =
            text_tab_bool_from_user_settings(&user_settings, TEXT_TAB_CENTERING_SHOW_CENTER_KEY, true);
        let cache_pages = project.canvas_settings.cache_pages;
        let residency = PageResidency::spawn(&project, cache_pages, Arc::clone(&memory_manager));
        // Warm the process-global text-render FontSystem pool off the GUI thread. The first lease
        // pays a full system-font scan (~2.2s in a fresh process); doing it here on a detached
        // background thread keeps it off the critical path so the user's first text render finds a
        // warm system. Guarded by `Once` because `new` can run multiple times per process
        // (launcher<->studio transitions) and we want at most one prewarm thread.
        spawn_text_render_font_pool_prewarm();
        let mut canvas = CanvasView::default();
        let loaded_bubble_mode = BubbleMode::from_str(&project.canvas_settings.bubble_type);
        canvas.state.bubble_mode = BubbleMode::Hybrid;
        canvas.state.hybrid_editable_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project.canvas_settings.editable_bubble_type)
                    .resolved(BubbleType::OnTop)
            }
        };
        canvas.state.hybrid_readonly_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project.canvas_settings.readonly_bubble_type)
                    .resolved(BubbleType::Aside)
            }
        };
        canvas.state.show_bubbles = project.canvas_settings.show_bubbles;
        canvas.state.show_bubble_status = project.canvas_settings.show_bubble_status;
        canvas.state.bubble_status_rules = project.canvas_settings.bubble_status_rules.clone();
        canvas.state.bubble_opacity = project.canvas_settings.bubble_opacity;
        canvas.state.bubble_min_width = project.canvas_settings.aside_min_width_px as f32;
        canvas.state.bubble_max_width = project.canvas_settings.aside_max_width_px as f32;
        canvas.state.aside_compact_mode =
            AsideBubbleCompactMode::from_str(&project.canvas_settings.aside_compact_mode);
        canvas.state.aside_side_mode =
            AsideBubbleSideMode::from_str(&project.canvas_settings.aside_side_mode);
        canvas.state.aside_second_column = project.canvas_settings.aside_second_column;
        canvas.state.on_top_focus_mode =
            OnTopFocusMode::from_str(&project.canvas_settings.on_top_focus_mode);
        canvas.state.scale_bubbles = project.canvas_settings.scale_bubbles;
        canvas.state.page_spacing = project.canvas_settings.page_spacing_px as f32;
        canvas.state.separate_pages = project.canvas_settings.separate_pages;
        canvas.state.edge_margin = project.canvas_settings.vertical_edge_margin_px as f32;
        canvas.state.side_margin = project.canvas_settings.side_margin_px as f32;
        canvas.state.aside_scale_pct = project.canvas_settings.aside_scale_pct;
        canvas.state.auto_insert_last_character =
            project.canvas_settings.auto_insert_last_character;
        canvas.state.spellcheck_original = project.canvas_settings.spellcheck_original;
        canvas.state.spellcheck_translation = project.canvas_settings.spellcheck_translation;
        canvas.state.tabs_autosync_enabled = project.canvas_settings.tabs_autosync_enabled;
        canvas.state.cache_pages = cache_pages;
        canvas.state.translation_status_display =
            TranslationStatusDisplay::from_str(&project.canvas_settings.translation_status_display);
        canvas.state.hint_show_outside_default =
            project.canvas_settings.hint_show_outside_default;
        canvas.set_bottom_hint_collapsed(translation_hint_collapsed);
        let shared_canvas_settings = canvas.canvas_snapshot();
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
        canvas.set_scroll_area_id_salt("translation_canvas_scroll");
        canvas.set_bubbles_model(Arc::clone(&bubbles_model));
        let shared_canvas_viewport = canvas.viewport_snapshot();
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
        let mut translation_tab = TranslationTabState::new(
            local_ai,
            Arc::clone(&ai_backend_health),
            ai_backend_probe_tx.clone(),
        );
        translation_tab.set_text_mask_model(Arc::clone(&text_mask_model));
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
            overlays.set_cache_pages_enabled(canvas.state.cache_pages);
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
        // Dockable-panel arrangement of this studio window, restored from the snapshot already in
        // hand. It must be installed BEFORE the first frame: the dock builds the default layout
        // for any key that has none, and the default is what the restored layout has to win over.
        let panel_dock = restore_panel_dock(&user_settings);
        typing_tab.set_hint_collapsed(typing_hint_collapsed);
        typing_tab.set_centering_assist_persisted_state(
            typing_centering_assist_enabled,
            typing_centering_show_center,
        );
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
        if let Some(layout) = settings_tab.take_typing_panel_layout_request() {
            typing_tab.set_panel_layout(layout);
        }
        // The one session-kind decision of `src/studio`: every per-kind behaviour below dispatches on it.
        let session = SessionMode::from_project(&project, &user_settings, has_staging);
        // The ONE push of what the session offers into the tabs that gate a feature on it. Once is
        // enough: the session kind never changes during a `StudioApp` lifetime. The Settings push
        // must follow `set_canvas_settings_binding` (it may clear the project spellcheck target).
        let session_caps = session.capabilities();
        translation_tab.set_session_capabilities(session_caps);
        cleaning_tab.set_session_capabilities(session_caps);
        typing_tab.set_session_capabilities(session_caps);
        settings_tab.set_session_capabilities(session_caps);
        let input_manager_v2 = build_input_manager_v2(session.hotkey_specs(), &hotkey_overrides);
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

        Self {
            project,
            residency,
            bubbles_model,
            clean_overlays_model,
            applied_bubbles_revision,
            cached_hotkey_hints: CachedHotkeyHints::default(),
            active_tab: AppTab::Translation,
            view_sync: ViewSync::new(shared_canvas_viewport),
            ai_backend_health,
            ai_backend_health_cached: AiBackendHealthSnapshot::default(),
            ai_backend_probe_tx: ai_backend_probe_tx.clone(),
            tabs: Tabs {
                translation_canvas: canvas,
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
            },
            panel_dock,
            panel_layout_writer: PanelLayoutWriter::spawn(),
            layer_doc,
            writers,
            input_manager_v2,
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
                translation_canvas: &mut self.tabs.translation_canvas,
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
            self.tabs.translation_canvas.state.aside_compact_mode =
                AsideBubbleCompactMode::from_str(aside_compact_mode);
            self.tabs.translation_canvas.state.separate_pages = separate_pages;
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

        let snapshot = self.tabs.translation_canvas.canvas_snapshot();

        if let Ok(mut bubbles_model) = self.bubbles_model.lock() {
            bubbles_model.set_canvas_settings(snapshot.clone());
        } else {
            runtime_log::log_warn(
                "[app] failed to lock BubblesModel while applying comic_type preset",
            );
            eprintln!("WARN app::apply_comic_type_preset: failed to lock BubblesModel");
        }
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
                self.tabs.translation.notify_characters_changed();
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

    fn dispatch_hotkeys(&mut self, ctx: &egui::Context) {
        let triggered_v2 = self
            .input_manager_v2
            .collect_triggered(ctx, self.active_tab);
        for command_id in triggered_v2 {
            self.execute_hotkey_command(ctx, &command_id);
        }
    }

    fn execute_hotkey_command(&mut self, ctx: &egui::Context, command_id: &str) {
        match command_id {
            // Registered only in an image-set session (`SessionMode::hotkey_specs`).
            image_set::HOTKEY_SAVE_IMAGE | image_set::HOTKEY_SAVE_IMAGE_AS => {
                self.session.on_save_hotkey(ctx, command_id);
            }
            HOTKEY_TRANSLATION_ZOOM_IN => {
                if !self.tabs.translation.blocks_canvas_zoom() {
                    self.tabs.translation_canvas.zoom_by_shortcut(1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_OUT => {
                if !self.tabs.translation.blocks_canvas_zoom() {
                    self.tabs.translation_canvas.zoom_by_shortcut(1.0 / 1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_RESET => {
                if !self.tabs.translation.blocks_canvas_zoom() {
                    self.tabs.translation_canvas.reset_zoom_shortcut();
                }
            }
            HOTKEY_TRANSLATION_DELETE_BUBBLE => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys() {
                    self.tabs.translation_canvas.delete_selected_bubble_shortcut();
                }
            }
            HOTKEY_TRANSLATION_CREATE_BUBBLE => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys()
                    && let Some(pointer_pos) = ctx.pointer_latest_pos()
                {
                    self.tabs.translation.create_bubble_at_pointer_shortcut(
                        ctx,
                        &mut self.tabs.translation_canvas,
                        &self.project,
                        pointer_pos,
                    );
                }
            }
            HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys()
                    && let Some(pointer_pos) = ctx.pointer_latest_pos()
                {
                    self.tabs.translation.create_hint_bubble_at_pointer_shortcut(
                        ctx,
                        &mut self.tabs.translation_canvas,
                        pointer_pos,
                    );
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys() {
                    self.tabs.translation_canvas
                        .copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys() {
                    self.tabs.translation_canvas
                        .copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys() {
                    self.tabs.translation_canvas
                        .paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION => {
                if !self.tabs.translation.blocks_canvas_bubble_hotkeys() {
                    self.tabs.translation_canvas
                        .paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL => {
                self.tabs.translation.toggle_bubbles_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL => {
                self.tabs.translation.toggle_ocr_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL => {
                self.tabs.translation.toggle_composition_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_MT_PANEL => {
                self.tabs.translation
                    .toggle_machine_translation_panel_hotkey();
            }
            HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL => {
                self.tabs.translation.toggle_text_detector_panel_hotkey();
            }
            HOTKEY_CLEANING_ZOOM_IN => {
                self.tabs.cleaning.zoom_by_shortcut(1.1);
            }
            HOTKEY_CLEANING_ZOOM_OUT => {
                self.tabs.cleaning.zoom_by_shortcut(1.0 / 1.1);
            }
            HOTKEY_CLEANING_ZOOM_RESET => {
                self.tabs.cleaning.reset_zoom_shortcut();
            }
            _ => {}
        }
    }

    fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        if let Some(canvas) = self.tabs.canvas(self.active_tab) {
            return canvas.active_source_page_window(neighbor_radius);
        }
        match self.active_tab {
            // Not reached: `view_sync::canvas_slot` maps these tabs to a canvas, answered above.
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing => HashSet::new(),
            // The page manager's viewer draws ONE page from these textures (lent in `draw`); its
            // window is exactly that page, so the per-frame trim neither drops nor thrashes it.
            AppTab::PageManager => self.tabs.page_manager.viewer_source_page().into_iter().collect(),
            // The PS-like editor owns its own page residency through `CleanOverlaysModel`'s page
            // cache, so it does not participate in the shared source-texture window.
            AppTab::PsEditor
            | AppTab::Characters
            | AppTab::Terms
            | AppTab::Notes
            | AppTab::Settings
            | AppTab::Wiki => HashSet::new(),
        }
    }

    fn active_canvas_source_pixel_inspection(&self) -> bool {
        if let Some(canvas) = self.tabs.canvas(self.active_tab) {
            return canvas.source_pixel_inspection_active();
        }
        match self.active_tab {
            // Not reached: `view_sync::canvas_slot` maps these tabs to a canvas, answered above.
            AppTab::Translation | AppTab::Cleaning | AppTab::Typing => false,
            // The viewer zoomed past its NEAREST threshold draws the page's nearest tiles.
            AppTab::PageManager => self.tabs.page_manager.viewer_wants_nearest_source(),
            AppTab::PsEditor
            | AppTab::Characters
            | AppTab::Terms
            | AppTab::Notes
            | AppTab::Settings
            | AppTab::Wiki => false,
        }
    }

    fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.tabs.translation_canvas.evict_clean_overlay_gpu_cache(request);
        let detector_report = self.tabs.translation.evict_text_detector_gpu_cache(request);
        append_eviction_report(&mut report, detector_report);
        let cleaning_overlay_report = self.tabs.cleaning.evict_clean_overlay_gpu_cache(request);
        append_eviction_report(&mut report, cleaning_overlay_report);
        let cleaning_mask_report = self.tabs.cleaning.evict_cleaning_mask_gpu_cache(request);
        append_eviction_report(&mut report, cleaning_mask_report);
        let typing_overlay_report = self.tabs.typing.evict_clean_overlay_gpu_cache(request);
        append_eviction_report(&mut report, typing_overlay_report);
        let typing_report = self.tabs.typing.evict_gpu_caches(request);
        append_eviction_report(&mut report, typing_report);
        report
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
        // `evict_tab_gpu_caches` builds its own per-tab sub-snapshots internally, so no
        // separate snapshot is computed here.
        if request.pressure != MemoryPressure::Normal {
            let report = self.evict_tab_gpu_caches(&request);
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
        append_eviction_report(&mut report, self.evict_tab_gpu_caches(&request));

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

    fn refresh_hotkey_hints_cache(&mut self, ctx: &egui::Context) {
        let bindings_revision = self.input_manager_v2.bindings_revision();
        if bindings_revision == self.cached_hotkey_hints.bindings_revision {
            return;
        }

        self.cached_hotkey_hints = CachedHotkeyHints {
            bindings_revision,
            create_bubble_hint: self
                .input_manager_v2
                .shortcut_text(ctx, HOTKEY_TRANSLATION_CREATE_BUBBLE),
            translation_hints: TranslationHotkeyHints {
                ocr_quick_selection_mode: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE),
                ocr_quick_selection_mode_modifier_down: false,
                ocr_advanced_selection_mode: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE),
                ocr_advanced_selection_mode_modifier_down: false,
                bubbles_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL),
                ocr_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL),
                composition_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL),
                machine_translation_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL),
                text_detector_panel: self
                    .input_manager_v2
                    .shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL),
            },
        };
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

        {
            // Coarse marker for the per-frame worker polling + GPU upload phase: decoded page
            // promotion, incremental texture upload, and overlay/page-cache loader draining.
            #[cfg(feature = "profiling")]
            puffin::profile_scope!("poll_loaders_and_upload");
            self.residency.poll_decoded(ctx, &self.clean_overlays_model);
            // Before any gate reads `clean_op_in_flight` (page ops, save-to-project), and whether
            // or not the page manager is the drawn tab.
            self.tabs.page_manager.poll_clean_events(ctx, &self.project);
            // Frees the page viewer's clean tiles and textures while the page manager is not the
            // drawn tab (they are prepared again when it is drawn next).
            self.tabs.page_manager.release_hidden_viewer_clean(ctx);
            self.residency.poll_page_cache(&self.project, self.tabs.translation_canvas.state.cache_pages, &self.clean_overlays_model);
        }
        self.sync_project_from_bubbles_model();
        self.refresh_hotkey_hints_cache(ctx);
        self.tabs.translation_canvas
            .set_create_bubble_shortcut_hint(self.cached_hotkey_hints.create_bubble_hint.clone());
        let mut translation_hotkey_hints = self.cached_hotkey_hints.translation_hints.clone();
        translation_hotkey_hints.ocr_quick_selection_mode_modifier_down = self
            .input_manager_v2
            .modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE);
        translation_hotkey_hints.ocr_advanced_selection_mode_modifier_down = self
            .input_manager_v2
            .modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE);
        self.tabs.translation
            .set_hotkey_hints(translation_hotkey_hints);
        self.tabs.apply_typing_panel_layout_request();
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
        let dock_pass = PanelDockPass::begin(drawn_tab);
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
                hotkeys: &mut self.input_manager_v2,
            },
        );
        // Panel-layout persistence poll, right after a dock host drew: the dock raises its dirty
        // flag DURING a gesture — on every frame a panel drag advances — so the flag must be picked
        // up in the same frame it was raised; the writer is what turns that per-frame stream into
        // one debounced write. Before the requests, as it ran before the typing deep link.
        if tab_hosts_panel_dock(drawn_tab)
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

        self.refresh_ai_backend_health_cache();
        self.ai_device_prompt.refresh(&self.ai_backend_health_cached, self.ai_backend_probe_tx.as_ref());
        if let Some(comic_type) = self.comic_type_prompt.draw(ctx) {
            self.apply_comic_type_preset(comic_type);
        }
        let ai_device_prompt_open = self.ai_device_prompt.draw(ctx, &self.ai_backend_health_cached, self.ai_backend_probe_tx.as_ref());
        if !self.comic_type_prompt.is_open() && !ai_device_prompt_open {
            self.dispatch_hotkeys(ctx);
        } else {
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
    /// Opens the pass for the tab that is about to draw. Call before `Tabs::draw`.
    #[must_use]
    fn begin(drawn_tab: AppTab) -> Self {
        Self {
            dock_ran: tab_hosts_panel_dock(drawn_tab),
        }
    }

    /// Whether `PanelDockState::show_idle_sub_windows` must run after the draw, i.e. whether the
    /// pass has not already shown the sub-window viewports.
    #[must_use]
    fn needs_idle_sub_windows(self) -> bool {
        !self.dock_ran
    }
}

/// Whether `tab` draws the app-owned panel dock, i.e. whether `PanelDock::end` runs while it is
/// the active program tab.
///
/// The single source of the "is this a dock host" question: it decides whether the dock's detached
/// sub-windows still need the idle keep-alive pass (`PanelDockState::show_idle_sub_windows`), which
/// must run on exactly the frames `PanelDock::end` does NOT. Adding a dock host means flipping its
/// arm here. The `match` is exhaustive with no catch-all so a new `AppTab` variant has to answer it.
#[must_use]
fn tab_hosts_panel_dock(tab: AppTab) -> bool {
    match tab {
        // The three canvas tabs: each declares the canvas' own «Лента» tab and further tabs
        // of its own — one for «Перевод», four for «Клининг», eight for «Текст». «PS редактор»
        // is the first NON-canvas host: it declares five tabs of its own and no «Лента».
        AppTab::Translation | AppTab::Cleaning | AppTab::Typing | AppTab::PsEditor => true,
        AppTab::PageManager
        | AppTab::Characters
        | AppTab::Terms
        | AppTab::Notes
        | AppTab::Settings
        | AppTab::Wiki => false,
    }
}

/// The default-layout builder of every program tab that hosts the panel dock, keyed by
/// `AppTab::key()`.
///
/// The dock migration's extension point: ONE entry per dock host, and the set of keys must match
/// [`tab_hosts_panel_dock`] exactly. A builder must name every `TabId` its tab can declare —
/// `persist` uses the default's tab set as the dictionary of known tabs, and a tab missing from it
/// would be dropped from the user's arrangement on every load.
///
/// Each entry must be the SAME `fn` item the owning tab hands to
/// `PanelDockState::ensure_default_layout` on every frame it draws: the state keeps that pointer to
/// rebuild the arrangement for the header's «сбросить раскладку» item, so two different builders for
/// one key would reset to something the user never saw. Pinned by
/// `tests::the_dock_hosts_register_the_builders_their_tabs_declare`.
///
/// Every canvas program tab needs a builder of its own — «Перевод» adds «Последние персонажи» on top
/// of the canvas' «Лента», «Клининг» adds four tabs and «Текст» eight — so there is no shared
/// ribbon-only builder any of them could fall back to.
#[must_use]
fn panel_dock_default_layout_builders() -> [panel_dock_persist::LayoutDefault<'static>; 4] {
    [
        (
            AppTab::Translation.key(),
            translation_default_dock_layout as fn() -> DockLayout,
        ),
        (
            AppTab::Cleaning.key(),
            cleaning_default_dock_layout as fn() -> DockLayout,
        ),
        (
            AppTab::Typing.key(),
            typing_default_dock_layout as fn() -> DockLayout,
        ),
        (
            AppTab::PsEditor.key(),
            ps_editor_default_dock_layout as fn() -> DockLayout,
        ),
    ]
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
/// The defaults come from [`panel_dock_default_layout_builders`], which is where a newly migrated
/// program tab is registered.
#[must_use]
fn restore_panel_dock(user_settings: &Value) -> PanelDockState {
    let restored = panel_dock_persist::layouts_from_user_settings(
        user_settings,
        &panel_dock_default_layout_builders(),
    );
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

/// Reads a per-tab canvas bottom-hint collapsed flag from the merged startup `user_settings`.
///
/// Looks up `Canvas.<key>` and returns its boolean value; a missing section, key, or non-boolean
/// value yields `false` (expanded), matching the persistence default.
#[must_use]
fn hint_collapsed_from_user_settings(user_settings: &Value, key: &str) -> bool {
    user_settings
        .get(CANVAS_CONFIG_SECTION)
        .and_then(Value::as_object)
        .and_then(|canvas| canvas.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Reads a `TextTab.<key>` boolean flag from the merged startup `user_settings`.
///
/// A missing section, missing key, or non-boolean value yields `default`, which differs per flag:
/// the centering-assist toggle seeds `false` (off) and the show-center marker seeds `true` (shown).
#[must_use]
fn text_tab_bool_from_user_settings(user_settings: &Value, key: &str, default: bool) -> bool {
    user_settings
        .get(TEXT_TAB_CONFIG_SECTION)
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

/// Inserts the per-tab canvas bottom-hint collapsed flags (Translation, Typing) into a
/// `user_config.json` root value, creating the `Canvas` object if absent and preserving all other
/// keys. Cleaning has no bottom-hint, so no cleaning flag is written; any stale
/// `cleaning_hint_collapsed` key is left untouched.
///
/// Pure and side-effect-free (the caller runs it inside `update_user_config_file`), so
/// the read-modify-merge contract is unit-testable without touching the filesystem. A non-object
/// `root` is replaced with a fresh object so the flags are never silently dropped.
fn apply_hint_collapsed_to_config_root(root: &mut Value, translation: bool, typing: bool) {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let Some(root_obj) = root.as_object_mut() else {
        return;
    };
    let mut canvas_obj = root_obj
        .get(CANVAS_CONFIG_SECTION)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    canvas_obj.insert(
        CANVAS_TRANSLATION_HINT_COLLAPSED_KEY.to_string(),
        Value::Bool(translation),
    );
    canvas_obj.insert(
        CANVAS_TYPING_HINT_COLLAPSED_KEY.to_string(),
        Value::Bool(typing),
    );
    root_obj.insert(
        CANVAS_CONFIG_SECTION.to_string(),
        Value::Object(canvas_obj),
    );
}

/// Persists the per-tab canvas bottom-hint collapsed flags (Translation, Typing) to
/// `user_config.json`.
///
/// Called once from [`StudioApp::shutdown_blocking`]. Routes through [`crate::config::update_user_config_file`],
/// the canonical serialized read-modify-write (atomic replace), so unrelated keys survive. A malformed
/// existing document is NOT overwritten: the error is logged and the file is left as is.
/// Teardown-only: on any read/parse/write failure it logs and returns without panicking, so a failed
/// write never blocks shutdown.
fn persist_canvas_hint_collapsed_state(translation: bool, typing: bool) {
    let path = crate::config::user_config_path();
    if let Err(err) = crate::config::update_user_config_file(&path, |root| {
        apply_hint_collapsed_to_config_root(root, translation, typing);
        Ok(())
    }) {
        runtime_log::log_error(format!(
            "[app::canvas_hint] failed to persist canvas hint collapsed state; file left unchanged; path={}; cause={err:#}",
            path.display()
        ));
    }
}

/// Inserts the two typing centering-assist flags (`centering_assist_enabled`, `centering_show_center`)
/// into a `user_config.json` root value, get-or-creating the `TextTab` object and preserving all other
/// keys. A non-object `root` or a non-object `TextTab` value is replaced with a fresh object so the
/// flags are never silently dropped. The bound-center KIND is session-only and is not written.
///
/// Pure and side-effect-free (the caller runs it inside `update_user_config_file`), so
/// the read-modify-merge contract is unit-testable without touching the filesystem.
fn apply_centering_assist_to_config_root(root: &mut Value, enabled: bool, show_center: bool) {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let Some(root_obj) = root.as_object_mut() else {
        return;
    };
    // Get-or-create the TextTab object; a non-object sibling value is replaced (not merged).
    let mut text_tab_obj = root_obj
        .get(TEXT_TAB_CONFIG_SECTION)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_tab_obj.insert(
        TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY.to_string(),
        Value::Bool(enabled),
    );
    text_tab_obj.insert(
        TEXT_TAB_CENTERING_SHOW_CENTER_KEY.to_string(),
        Value::Bool(show_center),
    );
    root_obj.insert(
        TEXT_TAB_CONFIG_SECTION.to_string(),
        Value::Object(text_tab_obj),
    );
}

/// Persists the two typing centering-assist flags to `user_config.json`.
///
/// Called once from [`StudioApp::shutdown_blocking`]. Routes through [`crate::config::update_user_config_file`],
/// the canonical locked read-modify-write primitive, so unrelated keys survive and the write is
/// serialized with every other config writer. Teardown-only: on any read/parse/write failure it logs a
/// warning and returns without panicking, so a failed write never blocks shutdown.
fn persist_typing_centering_assist_state(enabled: bool, show_center: bool) {
    let path = crate::config::user_config_path();
    if let Err(err) = crate::config::update_user_config_file(&path, |root| {
        apply_centering_assist_to_config_root(root, enabled, show_center);
        Ok(())
    }) {
        runtime_log::log_warn(format!(
            "[app::centering_assist] failed to persist centering-assist state; path={}; cause={err:#}",
            path.display()
        ));
    }
}

/// Builds the hotkey registry: every code-declared spec, then the session's own `session_specs`
/// (`SessionMode::hotkey_specs`: the image set's save hotkeys), then the pre-read user
/// `overrides` (`StudioBootstrap::hotkey_overrides`). No I/O.
fn build_input_manager_v2(session_specs: Vec<HotkeySpecV2>, overrides: &HotkeyOverrides) -> InputManagerV2 {
    let mut manager = InputManagerV2::default();
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_IN,
        title: t!("app.hotkey.zoom_in"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Equals,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_OUT,
        title: t!("app.hotkey.zoom_out"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Minus,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_ZOOM_RESET,
        title: t!("app.hotkey.zoom_reset"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Num0,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_DELETE_BUBBLE,
        title: t!("app.hotkey.delete_bubble"),
        section: t!("app.hotkey.bubbles_category"),
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::Delete,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_CREATE_BUBBLE,
        title: t!("app.hotkey.create_bubble"),
        section: t!("app.hotkey.bubbles_category"),
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::T,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE,
        title: t!("app.hotkey.create_hint_bubble"),
        section: t!("app.hotkey.bubbles_category"),
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::H,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Translation),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_IN,
        title: t!("app.hotkey.zoom_in"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Equals,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_OUT,
        title: t!("app.hotkey.zoom_out"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Minus,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    manager.register(HotkeySpecV2 {
        id: HOTKEY_CLEANING_ZOOM_RESET,
        title: t!("app.hotkey.zoom_reset"),
        section: "Canvas",
        default_shortcut: Some(egui::KeyboardShortcut::new(
            egui::Modifiers::COMMAND,
            egui::Key::Num0,
        )),
        default_modifier_only: None,
        scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
        active_when_input: false,
    });
    for spec in TranslationTabState::hotkey_specs() {
        manager.register(spec);
    }
    for spec in session_specs {
        manager.register(spec);
    }
    manager.apply_overrides(overrides);
    manager
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
    use super::{
        CANVAS_CONFIG_SECTION, CANVAS_TRANSLATION_HINT_COLLAPSED_KEY,
        CANVAS_TYPING_HINT_COLLAPSED_KEY,
        TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY, TEXT_TAB_CENTERING_SHOW_CENTER_KEY,
        TEXT_TAB_CONFIG_SECTION,
        apply_centering_assist_to_config_root, apply_hint_collapsed_to_config_root,
        hint_collapsed_from_user_settings,
        panel_dock_default_layout_builders,
        DockLayout, PanelDockPass,
        cleaning_default_dock_layout, tab_hosts_panel_dock, text_tab_bool_from_user_settings,
        ps_editor_default_dock_layout, translation_default_dock_layout, typing_default_dock_layout,
    };
    use crate::canvas::CANVAS_RIBBON_TAB;
    use crate::tabs::AppTab;
    use crate::widgets::panel_dock::TabId;
    use serde_json::{Value, json};

    /// The dock hosts are the tabs `PanelDock::end` runs for, and the idle sub-window keep-alive
    /// must run on exactly the complementary frames. The hosts are the three CANVAS tabs — each
    /// declares the canvas' own «Лента» tab — plus «PS редактор», which declares five tabs of its
    /// own over a full-area editing surface and no «Лента». A tab that starts hosting the dock has
    /// to be added here together with its default-layout entry in `restore_panel_dock`, while one
    /// that is added by accident shows up as a double-shown viewport.
    #[test]
    fn exactly_the_registered_tabs_host_the_panel_dock() {
        let hosts: Vec<AppTab> = AppTab::ALL
            .into_iter()
            .filter(|tab| tab_hosts_panel_dock(*tab))
            .collect();
        assert_eq!(
            hosts,
            vec![
                AppTab::Translation,
                AppTab::Cleaning,
                AppTab::Typing,
                AppTab::PsEditor
            ]
        );
    }

    /// A dock host with no entry in `panel_dock_default_layout_builders` loads with NO stored
    /// arrangement at all (`persist` only decodes the keys it is given), and an entry for a tab
    /// that does not host the dock is a builder nothing ever runs. The two lists are therefore one
    /// list, and this is the only place that says so.
    #[test]
    fn every_dock_host_has_exactly_one_default_layout_builder() {
        let mut registered: Vec<&str> = panel_dock_default_layout_builders()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let before_dedup = registered.len();
        registered.sort_unstable();
        registered.dedup();
        assert_eq!(
            registered.len(),
            before_dedup,
            "a program-tab key is registered twice: {registered:?}"
        );

        let mut hosts: Vec<&str> = AppTab::ALL
            .into_iter()
            .filter(|tab| tab_hosts_panel_dock(*tab))
            .map(AppTab::key)
            .collect();
        hosts.sort_unstable();
        assert_eq!(
            registered, hosts,
            "the registered default layouts and the dock hosts must be the same set"
        );
    }

    /// The contract the doc comments state as load-bearing: the builder registered here is the
    /// SAME `fn` item the owning tab hands to `ensure_default_layout` every frame, because the dock
    /// state keeps that pointer to serve the header's «сбросить раскладку» item.
    ///
    /// Checked twice on purpose. `fn_addr_eq` pins the identity itself, which is what the contract
    /// is about; the `TabId` set is the semantic backstop, since function-pointer equality is
    /// explicitly not guaranteed to survive codegen-unit merging and a builder swapped for one with
    /// a different tab dictionary is the failure that actually costs the user their arrangement.
    #[test]
    fn the_dock_hosts_register_the_builders_their_tabs_declare() {
        /// One row of the expectation table: the host tab, the builder it must be
        /// registered with, and the `TabId`s that builder's layout has to name.
        type ExpectedBuilder = (AppTab, fn() -> DockLayout, &'static [TabId]);

        // `const` items, not inline slice literals: rvalue static promotion does not
        // cover a `const fn` call, so an inline `&[TabId::new(..)]` would be a
        // temporary the `'static` row type cannot borrow.
        const TRANSLATION_TABS: &[TabId] = &[
            CANVAS_RIBBON_TAB,
            TabId::new("translation.recent_characters"),
        ];
        const CLEANING_TABS: &[TabId] = &[
            CANVAS_RIBBON_TAB,
            TabId::new("cleaning.clean"),
            TabId::new("cleaning.tools"),
            TabId::new("cleaning.active_tool"),
            TabId::new("cleaning.quick_clean"),
            TabId::new("cleaning.area_editor"),
        ];
        const TYPING_TABS: &[TabId] = &[
            CANVAS_RIBBON_TAB,
            TabId::new("typing.preview"),
            TabId::new("typing.params"),
            TabId::new("typing.effects"),
            TabId::new("typing.actions"),
            TabId::new("typing.layers"),
            TabId::new("typing.mask"),
            TabId::new("typing.deform"),
            TabId::new("typing.layout_editor"),
        ];

        const PS_EDITOR_TABS: &[TabId] = &[
            TabId::new("ps_editor.main"),
            TabId::new("ps_editor.tools"),
            TabId::new("ps_editor.active_tool"),
            TabId::new("ps_editor.hotkeys"),
            TabId::new("ps_editor.layers"),
            TabId::new("ps_editor.correction"),
        ];

        let expected: [ExpectedBuilder; 4] = [
            (
                AppTab::Translation,
                translation_default_dock_layout,
                TRANSLATION_TABS,
            ),
            (AppTab::Cleaning, cleaning_default_dock_layout, CLEANING_TABS),
            (AppTab::Typing, typing_default_dock_layout, TYPING_TABS),
            (
                AppTab::PsEditor,
                ps_editor_default_dock_layout,
                PS_EDITOR_TABS,
            ),
        ];
        let registered = panel_dock_default_layout_builders();
        for (tab, builder, tabs) in expected {
            let (_, found) = registered
                .into_iter()
                .find(|(key, _)| *key == tab.key())
                .unwrap_or_else(|| panic!("{} has no default-layout builder", tab.key()));
            assert!(
                std::ptr::fn_addr_eq(found, builder),
                "{} is registered with a builder other than the one its tab declares",
                tab.key()
            );
            let layout = found();
            for id in tabs {
                assert!(
                    layout.panel_of_tab(*id).is_some(),
                    "{}'s default layout does not name {id}, so a stored {id} would be dropped on \
                     every load",
                    tab.key()
                );
            }
        }
    }

    /// The idle sub-window pass must answer for the tab that DREW, and the two requests that
    /// reassign `active_tab` right after the draw are exactly what makes the two answers differ:
    /// «Менеджер страниц» opens a page in a canvas tab, and «Текст» deep-links into the
    /// settings tab. Reading the tab AFTER the requests therefore either skips the keep-alive
    /// (eframe drops the viewport and the window jumps when it is re-created) or runs it on
    /// top of `PanelDock::end` (one viewport shown twice in one pass).
    #[test]
    fn the_idle_sub_window_pass_answers_for_the_tab_that_drew() {
        // Not a dock host -> the keep-alive is required, whatever the pass ends on.
        let pass = PanelDockPass::begin(AppTab::PageManager);
        assert!(tab_hosts_panel_dock(AppTab::Typing), "the fixture needs a host tab");
        assert!(
            pass.needs_idle_sub_windows(),
            "the page-manager pass drew no dock, so the windows still need showing"
        );

        // A dock host -> `PanelDock::end` already showed the viewports; showing them again
        // in the same pass renders each of them twice.
        let pass = PanelDockPass::begin(AppTab::Typing);
        assert!(
            !tab_hosts_panel_dock(AppTab::Settings),
            "the fixture needs a non-host tab"
        );
        assert!(!pass.needs_idle_sub_windows());
    }

    #[test]
    fn apply_hint_collapsed_writes_two_flags_and_preserves_other_keys() {
        // A pre-existing config with an unrelated Canvas key, a stale cleaning-hint key, and an
        // unrelated section must all survive; only Translation + Typing flags are (re)written.
        let mut root = json!({
            "Canvas": { "cache_pages": true, "cleaning_hint_collapsed": true },
            "General": { "memory_profile": "Medium" }
        });
        apply_hint_collapsed_to_config_root(&mut root, true, true);

        let canvas = root
            .get(CANVAS_CONFIG_SECTION)
            .and_then(Value::as_object)
            .expect("Canvas section present");
        assert_eq!(
            canvas.get(CANVAS_TRANSLATION_HINT_COLLAPSED_KEY),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            canvas.get(CANVAS_TYPING_HINT_COLLAPSED_KEY),
            Some(&Value::Bool(true))
        );
        // Cleaning has no bottom-hint: no cleaning flag is written by us, but a stale one is left
        // untouched (no migration).
        assert_eq!(
            canvas.get("cleaning_hint_collapsed"),
            Some(&Value::Bool(true))
        );
        // Unrelated Canvas key and unrelated section untouched.
        assert_eq!(canvas.get("cache_pages"), Some(&Value::Bool(true)));
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get("memory_profile")),
            Some(&Value::String("Medium".to_string()))
        );
    }

    #[test]
    fn apply_hint_collapsed_replaces_non_object_root_and_creates_canvas() {
        // A corrupt (non-object) root is replaced with a fresh object; flags are never dropped.
        let mut root = Value::String("not an object".to_string());
        apply_hint_collapsed_to_config_root(&mut root, false, true);
        let canvas = root
            .get(CANVAS_CONFIG_SECTION)
            .and_then(Value::as_object)
            .expect("Canvas section created");
        assert_eq!(
            canvas.get(CANVAS_TRANSLATION_HINT_COLLAPSED_KEY),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            canvas.get(CANVAS_TYPING_HINT_COLLAPSED_KEY),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn hint_collapsed_defaults_to_false_when_absent_or_malformed() {
        // Missing section, missing key, and non-boolean value all yield the expanded default.
        assert!(!hint_collapsed_from_user_settings(
            &json!({}),
            CANVAS_TRANSLATION_HINT_COLLAPSED_KEY
        ));
        assert!(!hint_collapsed_from_user_settings(
            &json!({ "Canvas": {} }),
            CANVAS_TRANSLATION_HINT_COLLAPSED_KEY
        ));
        assert!(!hint_collapsed_from_user_settings(
            &json!({ "Canvas": { "translation_hint_collapsed": "yes" } }),
            CANVAS_TRANSLATION_HINT_COLLAPSED_KEY
        ));
        // A present true value round-trips.
        assert!(hint_collapsed_from_user_settings(
            &json!({ "Canvas": { "typing_hint_collapsed": true } }),
            CANVAS_TYPING_HINT_COLLAPSED_KEY
        ));
    }

    #[test]
    fn apply_centering_assist_writes_both_flags_and_preserves_siblings_and_sections() {
        // A pre-existing config with an unrelated TextTab sibling key and an unrelated section must all
        // survive; only the two centering-assist flags are (re)written.
        let mut root = json!({
            "TextTab": { "text_language": "en", "hanging_punctuation": true },
            "General": { "memory_profile": "Medium" }
        });
        apply_centering_assist_to_config_root(&mut root, true, false);

        let text_tab = root
            .get(TEXT_TAB_CONFIG_SECTION)
            .and_then(Value::as_object)
            .expect("TextTab section present");
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_SHOW_CENTER_KEY),
            Some(&Value::Bool(false))
        );
        // Unrelated TextTab keys and the unrelated section are untouched.
        assert_eq!(
            text_tab.get("text_language"),
            Some(&Value::String("en".to_string()))
        );
        assert_eq!(
            text_tab.get("hanging_punctuation"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|g| g.get("memory_profile")),
            Some(&Value::String("Medium".to_string()))
        );
    }

    #[test]
    fn apply_centering_assist_recovers_from_non_object_root_and_texttab() {
        // A corrupt (non-object) root is replaced with a fresh object; flags are never dropped.
        let mut root = Value::String("not an object".to_string());
        apply_centering_assist_to_config_root(&mut root, false, true);
        let text_tab = root
            .get(TEXT_TAB_CONFIG_SECTION)
            .and_then(Value::as_object)
            .expect("TextTab section created");
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_SHOW_CENTER_KEY),
            Some(&Value::Bool(true))
        );

        // A non-object TextTab value is replaced (not merged) so the flags still land.
        let mut root = json!({ "TextTab": "corrupt" });
        apply_centering_assist_to_config_root(&mut root, true, true);
        let text_tab = root
            .get(TEXT_TAB_CONFIG_SECTION)
            .and_then(Value::as_object)
            .expect("TextTab section replaced with object");
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            text_tab.get(TEXT_TAB_CENTERING_SHOW_CENTER_KEY),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn text_tab_bool_uses_asymmetric_defaults_when_absent_or_malformed() {
        // Absent section, absent key, and non-boolean values all yield the per-flag default.
        assert!(!text_tab_bool_from_user_settings(
            &json!({}),
            TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY,
            false
        ));
        assert!(text_tab_bool_from_user_settings(
            &json!({ "TextTab": {} }),
            TEXT_TAB_CENTERING_SHOW_CENTER_KEY,
            true
        ));
        assert!(text_tab_bool_from_user_settings(
            &json!({ "TextTab": { "centering_show_center": "yes" } }),
            TEXT_TAB_CENTERING_SHOW_CENTER_KEY,
            true
        ));
        // Present values override the default in both directions.
        assert!(text_tab_bool_from_user_settings(
            &json!({ "TextTab": { "centering_assist_enabled": true } }),
            TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY,
            false
        ));
        assert!(!text_tab_bool_from_user_settings(
            &json!({ "TextTab": { "centering_show_center": false } }),
            TEXT_TAB_CENTERING_SHOW_CENTER_KEY,
            true
        ));
    }
}
