/*
FILE OVERVIEW: src/tabs/settings/mod.rs
Settings tab state and shared runtime for settings subpanes.

Main types:
- `SettingsTabState`: active-section state + the shared `AiBackendHandle` it renders
  the shared panels against, the `SharedSettingsPanels` container that owns the three
  cross-surface panel states (General / AiBackend / Tutorials), and the user-facing
  memory profile binding to `MemoryManager`.

Section identity is the cross-surface `crate::settings_shared::SettingsSectionId`; the
tab bar and studio section order come from `settings_shared::sections_for(Studio)` and
labels from `settings_shared::title_key(id, Studio)`.

Flow:
- `draw`: renders the section switcher (from the shared registry) and dispatches. The
  shared sections (General / AiBackend / Tutorials) are rendered through
  `self.shared.draw(...)`; studio-only sections use this module's local renderers.
- The shared panels forward to the shared `crate::general_settings_panel` /
  `crate::ai_backend_panel` / `crate::tutorial` widgets over the app-global supervisor
  handle; the backend process/probe lifecycle itself lives in
  `crate::ai_backend_supervisor` (owned by `run_main`, not by this tab).
*/

mod canvas_ribbon;
mod general;
mod hotkeys;
mod typesetting;

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::bubble_status::BubbleStatusCondition;
use crate::canvas::{save_canvas_settings_to_project_file, save_canvas_settings_to_user_file};
use crate::config;
use crate::input_manager_v2::InputManagerV2;
use crate::memory_manager::{MemoryManager, MemoryProfile};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::{ComicType, save_comic_type_to_project_file};
use crate::runtime_log;
use crate::settings_shared::{
    SettingsDeepLink, SettingsSectionId, SettingsSurface, SharedSettingsPanels,
};
use crate::tabs::typing::TypingPanelLayout;
use crate::widgets::{
    current_spellcheck_words_revision, load_custom_spellcheck_words, load_project_spellcheck_words,
    save_custom_spellcheck_words, save_project_spellcheck_words,
    set_project_spellcheck_settings_file,
};
// The font-settings block's per-list name switch: the UI type lives with the widget, its
// `user_config.json` load/save with the other settings-tab config IO in this file.
use typesetting::{FontListKind, FontNameDisplayMode, FontNameDisplayModes};
// `Context` is what turns the `Option` of `as_object_mut` into a typed anyhow error
// inside `config::update_user_config_file`'s mutator.
use anyhow::Context;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread::{self as thread, JoinHandle};
use web_time::{Duration, Instant};

pub(super) const GENERAL_TYPING_PANEL_LAYOUT_KEY: &str = "typing_panel_layout";

#[derive(Debug, Clone)]
pub(super) struct DraggedBubbleConditionNode {
    pub(super) rule_id: u64,
    pub(super) path: Vec<usize>,
    pub(super) payload: BubbleStatusCondition,
}

#[derive(Debug)]
pub(super) struct CanvasSettingsRuntime {
    pub(super) tx: Sender<Option<CanvasSettingsSaveRequest>>,
    pub(super) thread: JoinHandle<()>,
    /// Word lists the saver failed to write; the GUI re-marks them unsynced so the next save
    /// retries them.
    pub(super) word_failures: Receiver<WordListSaveFailure>,
}

/// Which spellcheck word list a save concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WordList {
    /// The shared (user-level) custom dictionary.
    Custom,
    /// The current project's word list.
    Project,
}

/// A word list the saver thread could not write, with the exact text it tried to write.
#[derive(Debug)]
pub(super) struct WordListSaveFailure {
    pub(super) list: WordList,
    pub(super) words: String,
}

/// One canvas-settings save for the settings saver thread.
///
/// The two spellcheck word lists are `Some` only when the editor's text differs from what
/// this editor last loaded or sent ([`words_to_send`]): both lists are also written by the
/// spellcheck context menu's own worker, so re-sending an unchanged in-memory copy with
/// every canvas-settings save would overwrite a word added there in the meantime.
#[derive(Debug, Clone)]
pub(super) struct CanvasSettingsSaveRequest {
    pub(super) snapshot: SharedCanvasSettings,
    pub(super) comic_type: ComicType,
    pub(super) custom_spellcheck_words: Option<String>,
    pub(super) project_spellcheck_words: Option<String>,
}

impl CanvasSettingsSaveRequest {
    /// Coalesces a queued request with a `newer` one: the newer snapshot wins, and a word
    /// list the newer request does not carry keeps this request's pending edit.
    fn absorb_newer(self, newer: Self) -> Self {
        Self {
            snapshot: newer.snapshot,
            comic_type: newer.comic_type,
            custom_spellcheck_words: newer.custom_spellcheck_words.or(self.custom_spellcheck_words),
            project_spellcheck_words: newer.project_spellcheck_words.or(self.project_spellcheck_words),
        }
    }
}

/// The word list to send with a save: `Some(current)` when it differs from `synced` (the
/// text this editor last loaded or sent; `None` = unknown, e.g. after a failed write), which
/// then becomes `current`; `None` otherwise.
fn words_to_send(current: &str, synced: &mut Option<String>) -> Option<String> {
    if synced.as_deref() == Some(current) {
        return None;
    }
    *synced = Some(current.to_owned());
    Some(current.to_owned())
}

/// Reacts to a failed write of `failed`: when it is still the baseline (no newer text was
/// sent since), the baseline becomes unknown so the next save re-sends the list.
fn remark_unsynced_after_failure(synced: &mut Option<String>, failed: &str) {
    if synced.as_deref() == Some(failed) {
        *synced = None;
    }
}

#[derive(Debug)]
pub struct SettingsTabState {
    /// The currently displayed settings section (studio subset of the cross-surface
    /// `SettingsSectionId`). Only ids listed for `SettingsSurface::Studio` are ever
    /// set here; launcher-only ids are unreachable.
    active_pane: SettingsSectionId,
    user_settings_file: PathBuf,
    typing_panel_layout: TypingPanelLayout,
    pending_typing_panel_layout: Option<TypingPanelLayout>,
    memory_manager: Arc<MemoryManager>,
    /// Container owning the three cross-surface "double-interface" panel states
    /// (General / AiBackend / Tutorials), the same states the launcher settings page
    /// embeds. See `crate::settings_shared::SharedSettingsPanels`. The `AiBackendHandle`
    /// is NOT owned here; it is passed into `shared.draw` by reference from
    /// `ai_backend_handle`.
    shared: SharedSettingsPanels,
    hanging_punctuation_input: String,
    saved_hanging_punctuation: String,
    project_settings_file: PathBuf,
    canvas_settings: SharedCanvasSettings,
    bubbles_model: Option<Arc<Mutex<BubblesModel>>>,
    clean_overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    canvas_settings_runtime: Option<CanvasSettingsRuntime>,
    spellcheck_custom_words: String,
    project_spellcheck_custom_words: String,
    /// The shared dictionary text as last loaded from or sent to disk by this editor
    /// (`None`: a write failed, re-send at the next save).
    spellcheck_custom_words_synced: Option<String>,
    /// The project word list as last loaded from or sent to disk by this editor
    /// (`None`: a write failed, re-send at the next save).
    project_spellcheck_words_synced: Option<String>,
    spellcheck_words_revision_seen: u64,
    ai_backend_handle: AiBackendHandle,
    dragged_bubble_condition_node: Option<DraggedBubbleConditionNode>,
    hotkey_capture_command_id: Option<String>,
    /// Editor for per-effect-kind default parameters, shown in the "Тайп" pane.
    /// Self-contained typing-panel widget (double-interface pattern like
    /// `ai_backend_panel`): the effect model stays encapsulated behind this one
    /// public type; it reads/writes the runtime-global effect-defaults store and
    /// persists to `TextTab.effect_defaults` on its own background thread.
    effect_defaults_editor: crate::tabs::typing::EffectDefaultsEditorState,
    /// Editor for the "Настройки шрифтов" block, shown in the "Тайп" pane. Self-contained
    /// settings-local widget (double-interface pattern): it loads the font category
    /// lists off-thread, renders each font in its own typeface, and drives the
    /// runtime-global imported-fonts store for system-font import/removal — all through the
    /// `crate::tabs::typing::font_admin` facade, so settings needs no access to the private
    /// font model.
    font_settings_editor: typesetting::FontSettingsEditorState,
    /// Pending in-app deep-link reveal target set by [`SettingsTabState::navigate_to`].
    /// Consumed by the matching section draw (font-groups reveal in `draw_typesetting`)
    /// once the target block has actually RENDERED: while pending, the draw force-opens
    /// the target collapsed blocks and issues the scroll; the flag clears on the first
    /// frame the block draws (the font categories load asynchronously, so this may take a
    /// few frames on first visit), after which the user can freely collapse the blocks.
    pending_reveal: Option<SettingsDeepLink>,
    /// Give-up deadline for [`Self::pending_reveal`]: set lazily on the first frame the
    /// reveal has to WAIT for the async font-category load; if the target block still has
    /// not rendered by this instant, the pending reveal is abandoned so the force-open can
    /// never stick indefinitely. `None` while no wait is in progress.
    pending_reveal_expires: Option<Instant>,
    /// Deadline until which the freshly-revealed groups block is highlighted with an
    /// outline. Set when a reveal fires; `None` when no highlight is active. Uses
    /// `web_time::Instant` so it also works under wasm.
    reveal_highlight_until: Option<Instant>,
    /// `true` in a single-image session (`set_single_image_session`): the canvas pane hides the
    /// comic-type preset and the project word list, whose project is a throwaway scratch chapter.
    single_image_session: bool,
}

impl Default for SettingsTabState {
    fn default() -> Self {
        Self::new(AiBackendHandle::disabled(), Arc::new(MemoryManager::default()))
    }
}

impl SettingsTabState {
    pub fn new(ai_backend_handle: AiBackendHandle, memory_manager: Arc<MemoryManager>) -> Self {
        let user_settings_file = config::user_config_path();
        let typing_panel_layout = load_typing_panel_layout(&user_settings_file);
        // Interface preference of the font-settings lists; read here (once, at construction,
        // like the panel layout above) so the widget itself performs no I/O and still knows
        // where to write a change back to.
        let font_name_display_modes = load_font_name_display_modes(&user_settings_file);
        let font_settings_file = user_settings_file.clone();
        let shared = SharedSettingsPanels::new(
            #[cfg(feature = "tutorial")]
            crate::tutorial::shared_progress(),
        );
        // Seed the runtime memory profile from the shared general panel (loaded from
        // config), the same value the shared widget starts with.
        memory_manager.set_profile(shared.memory_profile());
        // Триггерит ленивую загрузку набора из конфига и даёт текущее значение.
        let hanging_punctuation = crate::text_punctuation::hanging_punctuation_string();

        Self {
            active_pane: SettingsSectionId::General,
            user_settings_file,
            typing_panel_layout,
            pending_typing_panel_layout: Some(typing_panel_layout),
            memory_manager,
            shared,
            hanging_punctuation_input: hanging_punctuation.clone(),
            saved_hanging_punctuation: hanging_punctuation,
            project_settings_file: PathBuf::new(),
            canvas_settings: SharedCanvasSettings::default(),
            bubbles_model: None,
            clean_overlays_model: None,
            canvas_settings_runtime: None,
            spellcheck_custom_words: String::new(),
            project_spellcheck_custom_words: String::new(),
            spellcheck_custom_words_synced: Some(String::new()),
            project_spellcheck_words_synced: Some(String::new()),
            spellcheck_words_revision_seen: current_spellcheck_words_revision(),
            ai_backend_handle,
            dragged_bubble_condition_node: None,
            hotkey_capture_command_id: None,
            effect_defaults_editor: crate::tabs::typing::EffectDefaultsEditorState::new(),
            font_settings_editor: typesetting::FontSettingsEditorState::new(
                font_settings_file,
                font_name_display_modes,
            ),
            pending_reveal: None,
            pending_reveal_expires: None,
            reveal_highlight_until: None,
            single_image_session: false,
        }
    }

    /// Applies an in-app settings deep link: selects the target section and stashes a
    /// pending reveal that the section's own draw consumes once the target block has
    /// rendered (force-opens the relevant collapsed blocks, scrolls to them, and
    /// highlights them for ~2 seconds).
    ///
    /// This does NOT switch the active app tab; the caller (`app.rs`) is responsible for
    /// flipping to the settings tab after calling this.
    pub fn navigate_to(&mut self, link: SettingsDeepLink) {
        match link {
            SettingsDeepLink::TypesettingFontGroups => {
                self.active_pane = SettingsSectionId::Typesetting;
                self.pending_reveal = Some(link);
                self.pending_reveal_expires = None;
            }
        }
    }
}

impl SettingsTabState {
    pub fn set_canvas_settings_binding(
        &mut self,
        project_settings_file: PathBuf,
        initial_canvas_settings: SharedCanvasSettings,
        bubbles_model: Arc<Mutex<BubblesModel>>,
        clean_overlays_model: Arc<Mutex<CleanOverlaysModel>>,
    ) {
        if let Some(runtime) = self.canvas_settings_runtime.take() {
            let _ = runtime.tx.send(None);
            let _ = runtime.thread.join();
        }

        self.project_settings_file = project_settings_file.clone();
        self.canvas_settings = initial_canvas_settings;
        set_project_spellcheck_settings_file(Some(project_settings_file.clone()));
        self.spellcheck_custom_words = load_custom_spellcheck_words().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[settings] failed to load custom spellcheck dictionary: {err}"
            ));
            String::new()
        });
        self.project_spellcheck_custom_words =
            load_project_spellcheck_words(&project_settings_file).unwrap_or_else(|err| {
                runtime_log::log_warn(format!(
                    "[settings] failed to load project spellcheck words '{}': {err}",
                    project_settings_file.display()
                ));
                String::new()
            });
        self.spellcheck_custom_words_synced = Some(self.spellcheck_custom_words.clone());
        self.project_spellcheck_words_synced = Some(self.project_spellcheck_custom_words.clone());
        self.spellcheck_words_revision_seen = current_spellcheck_words_revision();
        self.bubbles_model = Some(bubbles_model);
        self.clean_overlays_model = Some(clean_overlays_model);
        self.apply_memory_profile_to_runtime(self.shared.memory_profile());
        self.canvas_settings_runtime = Some(spawn_canvas_settings_save_worker(
            self.user_settings_file.clone(),
            project_settings_file,
        ));
    }

    /// Marks this tab as part of a single-image session. Call after
    /// [`Self::set_canvas_settings_binding`]: the bound "project" is the session's scratch chapter,
    /// so the spellcheck context menu loses its "add to the project dictionary" target, and the
    /// canvas pane hides the comic-type preset and the project word list (plan
    /// `dev-docs/single_image_mode_plan.md`, WP-2.4). User-level settings stay editable.
    pub fn set_single_image_session(&mut self) {
        self.single_image_session = true;
        set_project_spellcheck_settings_file(None);
    }

    pub fn take_typing_panel_layout_request(&mut self) -> Option<TypingPanelLayout> {
        self.pending_typing_panel_layout.take()
    }

    pub fn draw(&mut self, ui: &mut egui::Ui, hotkeys_v2: &mut InputManagerV2) {
        let process_running = self.ai_backend_handle.process_snapshot().running();
        ui.heading(t!("settings.nav.title"));
        ui.horizontal_wrapped(|ui| {
            // Section list + order come from the shared registry (studio subset). The
            // label is the surface-specific localization key resolved at runtime, since
            // the key is dynamic (`title_key` is not a `t!` literal).
            for descriptor in crate::settings_shared::sections_for(SettingsSurface::Studio) {
                let id = descriptor.id;
                let key = crate::settings_shared::title_key(id, SettingsSurface::Studio);
                let label = ms_i18n::lookup(key).unwrap_or(key);
                if ui.selectable_label(self.active_pane == id, label).clicked() {
                    self.active_pane = id;
                }
            }
        });
        ui.separator();

        match self.active_pane {
            SettingsSectionId::General => self.draw_general(ui),
            SettingsSectionId::CanvasRibbon => self.draw_canvas_ribbon(ui),
            SettingsSectionId::Typesetting => self.draw_typesetting(ui),
            SettingsSectionId::AiBackend => {
                // Studio wraps the shared AI backend panel in a scroll area (it is
                // taller than the settings viewport, like the launcher settings page);
                // `auto_shrink` off so it fills the available space. The section is
                // shared and produces no studio runtime outcome, so its result is
                // intentionally discarded.
                egui::ScrollArea::vertical()
                    .id_salt("settings_ai_backend_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let _ = self.shared.draw(
                            SettingsSectionId::AiBackend,
                            ui,
                            SettingsSurface::Studio,
                            &self.ai_backend_handle,
                        );
                    });
            }
            SettingsSectionId::Hotkeys => self.draw_hotkeys(ui, hotkeys_v2),
            #[cfg(feature = "tutorial")]
            SettingsSectionId::Tutorials => {
                // Shared tutorials pane; no studio runtime outcome to apply.
                let _ = self.shared.draw(
                    SettingsSectionId::Tutorials,
                    ui,
                    SettingsSurface::Studio,
                    &self.ai_backend_handle,
                );
            }
            // Launcher-only sections can never be the studio's active pane: the tab bar
            // only offers `sections_for(Studio)`, and `active_pane` is only ever set to
            // one of those ids. Kept exhaustive (no `_ =>`) so a new section forces a
            // decision here.
            SettingsSectionId::SystemInfo
            | SettingsSectionId::AiComputations
            | SettingsSectionId::TorchUpgrade
            | SettingsSectionId::PythonEnvironment => {
                debug_assert!(
                    false,
                    "studio settings active_pane is a launcher-only section: {:?}",
                    self.active_pane
                );
            }
        }

        let repaint_after = if process_running {
            Duration::from_millis(120)
        } else {
            Duration::from_millis(350)
        };
        ui.ctx().request_repaint_after(repaint_after);
    }
}

impl SettingsTabState {
    fn publish_canvas_settings(&mut self) {
        let comic_type = ComicType::from_canvas_preset_fields(
            &self.canvas_settings.aside_compact_mode,
            self.canvas_settings.separate_pages,
        );

        if let Some(model) = self.bubbles_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_canvas_settings(self.canvas_settings.clone()),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock BubblesModel while publishing canvas settings",
                ),
            }
        }

        if let Some(model) = self.clean_overlays_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_cache_pages_enabled(self.canvas_settings.cache_pages),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock CleanOverlaysModel while syncing cache_pages",
                ),
            }
        }

        if let Some(runtime) = self.canvas_settings_runtime.as_ref() {
            // Failed word-list writes reported since the last save are re-sent with this one.
            for failure in runtime.word_failures.try_iter() {
                let synced = match failure.list {
                    WordList::Custom => &mut self.spellcheck_custom_words_synced,
                    WordList::Project => &mut self.project_spellcheck_words_synced,
                };
                remark_unsynced_after_failure(synced, &failure.words);
            }
            let request = CanvasSettingsSaveRequest {
                snapshot: self.canvas_settings.clone(),
                comic_type,
                custom_spellcheck_words: words_to_send(&self.spellcheck_custom_words, &mut self.spellcheck_custom_words_synced),
                project_spellcheck_words: words_to_send(&self.project_spellcheck_custom_words, &mut self.project_spellcheck_words_synced),
            };
            if runtime.tx.send(Some(request)).is_err() {
                runtime_log::log_error("[settings] canvas settings saver thread is gone; this save was not written");
            }
        }
    }

    pub fn replace_canvas_settings_from_snapshot(&mut self, snapshot: SharedCanvasSettings) {
        self.canvas_settings = snapshot;
    }

    pub fn persist_canvas_settings(&mut self) {
        self.publish_canvas_settings();
    }

    pub(super) fn apply_memory_profile_to_runtime(&self, profile: MemoryProfile) {
        self.memory_manager.set_profile(profile);
        if let Some(model) = self.clean_overlays_model.as_ref() {
            match model.lock() {
                Ok(mut guard) => guard.set_memory_profile(profile),
                Err(_) => runtime_log::log_warn(
                    "[settings] failed to lock CleanOverlaysModel while applying memory profile",
                ),
            }
        }
    }

    fn refresh_spellcheck_words_if_needed(&mut self) {
        let current_revision = current_spellcheck_words_revision();
        if current_revision == self.spellcheck_words_revision_seen {
            return;
        }

        self.spellcheck_custom_words = load_custom_spellcheck_words().unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[settings] failed to refresh custom spellcheck dictionary: {err}"
            ));
            String::new()
        });
        self.project_spellcheck_custom_words =
            load_project_spellcheck_words(&self.project_settings_file).unwrap_or_else(|err| {
                runtime_log::log_warn(format!(
                    "[settings] failed to refresh project spellcheck words '{}': {err}",
                    self.project_settings_file.display()
                ));
                String::new()
            });
        self.spellcheck_custom_words_synced = Some(self.spellcheck_custom_words.clone());
        self.project_spellcheck_words_synced = Some(self.project_spellcheck_custom_words.clone());
        self.spellcheck_words_revision_seen = current_revision;
    }
}

impl Drop for SettingsTabState {
    fn drop(&mut self) {
        set_project_spellcheck_settings_file(None);
        if let Some(runtime) = self.canvas_settings_runtime.take() {
            let _ = runtime.tx.send(None);
            let _ = runtime.thread.join();
        }
    }
}

/// Spawns the settings saver thread: it persists each (coalesced) canvas-settings request
/// to the project and user config and writes the word lists a request carries, reporting a
/// failed word-list write on `word_failures` so the GUI retries it with the next save.
fn spawn_canvas_settings_save_worker(
    user_settings_file: PathBuf,
    project_settings_file: PathBuf,
) -> CanvasSettingsRuntime {
    let (tx, rx) = mpsc::channel::<Option<CanvasSettingsSaveRequest>>();
    let (failure_tx, word_failures) = mpsc::channel::<WordListSaveFailure>();
    let thread = thread::spawn(move || {
        let report_failure = |list: WordList, words: &str| {
            // A closed receiver only means the settings tab is gone; nobody is left to retry.
            if failure_tx.send(WordListSaveFailure { list, words: words.to_owned() }).is_err() {
                runtime_log::log_warn("[settings] word-list save failure has no receiver; it will not be retried");
            }
        };
        run_canvas_settings_saver(&rx, |latest| {
            if !project_settings_file.as_os_str().is_empty() {
                if let Err(err) =
                    save_canvas_settings_to_project_file(&project_settings_file, &latest.snapshot)
                {
                    runtime_log::log_error(format!(
                        "[settings] failed to persist project canvas settings {}; error={err}",
                        project_settings_file.display()
                    ));
                }

                if let Err(err) =
                    save_comic_type_to_project_file(&project_settings_file, latest.comic_type, ms_docstore::Durability::Contents)
                {
                    runtime_log::log_error(format!(
                        "[settings] failed to persist comic_type='{}' to {}; error={err}",
                        latest.comic_type.as_config_str(),
                        project_settings_file.display()
                    ));
                }
            }

            if let Err(err) =
                save_canvas_settings_to_user_file(&user_settings_file, &latest.snapshot)
            {
                runtime_log::log_error(format!(
                    "[settings] failed to persist user canvas settings {}; error={err}",
                    user_settings_file.display()
                ));
            }

            if let Some(words) = &latest.custom_spellcheck_words
                && let Err(err) = save_custom_spellcheck_words(words)
            {
                runtime_log::log_error(format!(
                    "[settings] failed to persist custom spellcheck dictionary; it is retried with the next settings save; error={err}"
                ));
                report_failure(WordList::Custom, words);
            }

            if !project_settings_file.as_os_str().is_empty()
                && let Some(words) = &latest.project_spellcheck_words
                && let Err(err) = save_project_spellcheck_words(&project_settings_file, words)
            {
                runtime_log::log_error(format!(
                    "[settings] failed to persist project spellcheck words '{}'; it is retried with the next settings save; error={err}",
                    project_settings_file.display()
                ));
                report_failure(WordList::Project, words);
            }
        });
    });

    CanvasSettingsRuntime { tx, thread, word_failures }
}

/// The saver loop: blocks for a request, coalesces every request already queued behind it
/// ([`CanvasSettingsSaveRequest::absorb_newer`]) and hands the result to `save`. A `None`
/// (shutdown / rebind) or a closed channel ends the loop — but a `None` found while
/// coalescing ends it only AFTER the requests merged before it were saved, so the final
/// save before a shutdown is never dropped.
fn run_canvas_settings_saver(rx: &Receiver<Option<CanvasSettingsSaveRequest>>, mut save: impl FnMut(&CanvasSettingsSaveRequest)) {
    while let Ok(first) = rx.recv() {
        let Some(mut latest) = first else {
            break;
        };
        let mut stop = false;
        while let Ok(next) = rx.try_recv() {
            match next {
                Some(request) => latest = latest.absorb_newer(request),
                None => {
                    stop = true;
                    break;
                }
            }
        }
        save(&latest);
        if stop {
            break;
        }
    }
}

/// Upserts one string value `section.key = value` into `user_config.json`, preserving every
/// other key, in ONE serialized read-modify-write ([`config::update_user_config_file`]: atomic
/// replace, serialized with every other config writer). A non-object `section` value is replaced
/// by a fresh object. A malformed existing document is reported and left untouched.
///
/// Performs synchronous disk I/O — call it from a worker thread, never from the GUI thread.
///
/// # Errors
/// A user-facing string with the read/parse/write failure and the path.
fn upsert_user_config_string(
    user_settings_file: &Path,
    section: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    config::update_user_config_file(user_settings_file, |root| {
        // `update_user_config_file` guarantees an object root before the mutator runs.
        let root_obj = root
            .as_object_mut()
            .context(t!("settings.config_io.prepare_root_error").to_string())?;
        let mut section_obj = root_obj
            .get(section)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        section_obj.insert(key.to_string(), Value::String(value.to_string()));
        root_obj.insert(section.to_string(), Value::Object(section_obj));
        Ok(())
    })
    .map_err(|err| format!("{err:#}"))
}

/// Reads the `user_config.json` root for a best-effort preference lookup: a missing,
/// unreadable or malformed document yields `None` (the caller then uses its default).
fn read_user_config_root_or_none(user_settings_file: &Path) -> Option<Value> {
    read_user_config_root(user_settings_file).ok()
}

/// Reads every switchable font surface's name-display mode from `user_config.json`
/// (`TextTab.font_list_name_mode_folder` / `…_imported` / `…_group`).
///
/// A missing file, unparsable JSON, an absent key or an unrecognized token all yield
/// [`FontNameDisplayMode::Custom`] for that surface — the historical behavior. Like
/// [`load_typing_panel_layout`] this performs one blocking read, so call it at construction
/// time or off the GUI thread, never per frame.
#[must_use]
pub(super) fn load_font_name_display_modes(user_settings_file: &Path) -> FontNameDisplayModes {
    let Some(payload) = read_user_config_root_or_none(user_settings_file) else {
        return FontNameDisplayModes::default();
    };
    let text_tab = payload.get("TextTab").and_then(Value::as_object);
    let mode_of = |list: FontListKind| -> FontNameDisplayMode {
        text_tab
            .and_then(|obj| obj.get(list.config_key()))
            .and_then(Value::as_str)
            .and_then(FontNameDisplayMode::from_config_str)
            .unwrap_or_default()
    };
    FontNameDisplayModes {
        folder: mode_of(FontListKind::Folder),
        imported: mode_of(FontListKind::Imported),
        group: mode_of(FontListKind::Group),
    }
}

/// Persists ONE font surface's name-display mode under its `TextTab` key in
/// `user_config.json`, preserving every other key.
///
/// One serialized read-modify-write (see [`upsert_user_config_string`]), so a background
/// save never clobbers a concurrent one. Performs synchronous disk I/O — call it from a
/// worker thread, never from the GUI thread.
///
/// # Errors
/// A user-facing error string describing what failed; a malformed file is left untouched.
pub(super) fn save_font_name_display_mode(
    user_settings_file: &Path,
    list: FontListKind,
    mode: FontNameDisplayMode,
) -> Result<(), String> {
    upsert_user_config_string(user_settings_file, "TextTab", list.config_key(), mode.as_config_str())
}

/// Reads the typing panel layout (`General.typing_panel_layout`) from `user_config.json`.
/// A missing/malformed document, absent key or unknown token yields
/// [`TypingPanelLayout::Vertical`]. One blocking read — never call it per frame.
pub(super) fn load_typing_panel_layout(user_settings_file: &Path) -> TypingPanelLayout {
    let Some(payload) = read_user_config_root_or_none(user_settings_file) else {
        return TypingPanelLayout::Vertical;
    };
    payload
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(GENERAL_TYPING_PANEL_LAYOUT_KEY))
        .and_then(Value::as_str)
        .and_then(TypingPanelLayout::from_config_str)
        .unwrap_or(TypingPanelLayout::Vertical)
}

/// Persists the typing panel layout to `General.typing_panel_layout` in `user_config.json`
/// (one serialized read-modify-write, other keys preserved). Worker thread only.
///
/// # Errors
/// A user-facing error string; a malformed file is reported and left untouched.
pub(super) fn save_typing_panel_layout(
    user_settings_file: &Path,
    layout: TypingPanelLayout,
) -> Result<(), String> {
    upsert_user_config_string(user_settings_file, "General", GENERAL_TYPING_PANEL_LAYOUT_KEY, layout.as_config_str())
}

/// Persists the rotation Ctrl+wheel mode to `TextTab.rotation_ctrl_wheel_mode` in
/// `user_config.json` (one serialized read-modify-write, other keys preserved). Worker
/// thread only.
///
/// # Errors
/// A user-facing error string; a malformed file is reported and left untouched.
pub(super) fn save_rotation_ctrl_wheel_mode(
    user_settings_file: &Path,
    mode: crate::tabs::typing::rotation_ctrl_wheel::RotationCtrlWheelMode,
) -> Result<(), String> {
    upsert_user_config_string(user_settings_file, "TextTab", config::TEXT_TAB_ROTATION_CTRL_WHEEL_MODE_KEY, mode.as_config_str())
}

// The `General.ort_load_state` crash guard (`mark_ort_load_attempted` /
// `mark_ort_load_succeeded` / `reset_ort_load_guard`, plus the shared
// `read_user_config_root` the section writers of this module use) is DECLARED in
// `ms_config::ort_load_guard`. It had to move down: its writer is the native ONNX
// Runtime loader (crate `ms-native-runtime`), which may not depend on this tab, while
// its reader (`config::read_ort_load_guard`) was already in `ms-config`. The only
// surface offering the "Повторить попытку ORT" control is the AI-backend panel (crate
// `ms-settings-ui`), which calls `ms_config::ort_load_guard::reset_ort_load_guard`
// directly — nothing here re-exports it any more.
use config::ort_load_guard::read_user_config_root;

// The `user_config.json` section writers this surface shares with the AI-backend panel
// and the general pane (`save_text_language`, `save_ai_runtime`,
// `save_onnx_provider_device`, `save_onnx_build`, `save_max_loaded_models`) are DECLARED
// in `ms-config`, next to `save_advanced_form_search_params`. They had to move down:
// those two panels live in `ms-settings-ui`, are shared with the launcher's settings
// page, and may not depend on this tab. `save_hanging_punctuation` moved with them (same
// family, same file) and is re-exported here because the typesetting pane still calls it.
pub(super) use config::save_hanging_punctuation;


#[cfg(test)]
mod font_name_mode_tests {
    use super::*;
    use std::fs;

    /// Reads back a written config file as JSON.
    fn read_root(path: &Path) -> Value {
        let raw = fs::read_to_string(path).expect("config file written");
        serde_json::from_str::<Value>(&raw).expect("config file is valid json")
    }

    #[test]
    fn missing_config_reads_as_the_default_mode() {
        let temp = tempfile::tempdir().expect("temp dir");
        // Nothing written yet: both lists must read as the historical behavior.
        let path = temp.path().join("user_config.json");
        assert_eq!(
            load_font_name_display_modes(&path),
            FontNameDisplayModes::default()
        );
        assert_eq!(
            load_font_name_display_modes(&path).folder,
            FontNameDisplayMode::Custom
        );
    }

    #[test]
    fn unparsable_or_unknown_values_read_as_the_default_mode() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");

        fs::write(&path, "{ this is not json").expect("write garbage");
        assert_eq!(
            load_font_name_display_modes(&path),
            FontNameDisplayModes::default()
        );

        fs::write(
            &path,
            r#"{"TextTab":{"font_list_name_mode_folder":"postscript"}}"#,
        )
        .expect("write unknown token");
        assert_eq!(
            load_font_name_display_modes(&path),
            FontNameDisplayModes::default()
        );
    }

    #[test]
    fn each_list_persists_its_own_mode_independently() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");

        save_font_name_display_mode(&path, FontListKind::Imported, FontNameDisplayMode::Identity)
            .expect("save imported mode");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.imported, FontNameDisplayMode::Identity);
        // The other surfaces keep the default until they are switched themselves.
        assert_eq!(modes.folder, FontNameDisplayMode::Custom);
        assert_eq!(modes.group, FontNameDisplayMode::Custom);

        save_font_name_display_mode(&path, FontListKind::Folder, FontNameDisplayMode::Identity)
            .expect("save folder mode");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.folder, FontNameDisplayMode::Identity);
        assert_eq!(modes.imported, FontNameDisplayMode::Identity);
        assert_eq!(modes.group, FontNameDisplayMode::Custom);

        // Switching back is persisted too (the value is written, not just added once).
        save_font_name_display_mode(&path, FontListKind::Folder, FontNameDisplayMode::Custom)
            .expect("save folder mode back");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.folder, FontNameDisplayMode::Custom);
        assert_eq!(modes.imported, FontNameDisplayMode::Identity);
    }

    #[test]
    fn the_group_editor_mode_persists_separately_from_both_lists() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");

        save_font_name_display_mode(&path, FontListKind::Group, FontNameDisplayMode::Identity)
            .expect("save group mode");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.group, FontNameDisplayMode::Identity);
        // The group-editor window has its own key: neither category list moved with it.
        assert_eq!(modes.folder, FontNameDisplayMode::Custom);
        assert_eq!(modes.imported, FontNameDisplayMode::Custom);

        // And switching a list back does not disturb the group window's stored choice.
        save_font_name_display_mode(&path, FontListKind::Folder, FontNameDisplayMode::Identity)
            .expect("save folder mode");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.group, FontNameDisplayMode::Identity);
        assert_eq!(modes.folder, FontNameDisplayMode::Identity);

        save_font_name_display_mode(&path, FontListKind::Group, FontNameDisplayMode::Custom)
            .expect("save group mode back");
        let modes = load_font_name_display_modes(&path);
        assert_eq!(modes.group, FontNameDisplayMode::Custom);
        assert_eq!(modes.folder, FontNameDisplayMode::Identity);
    }

    #[test]
    fn saving_a_mode_preserves_unrelated_config_keys() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");
        fs::write(
            &path,
            r#"{"General":{"ui_language":"ru"},"TextTab":{"hanging_punctuation":",."}}"#,
        )
        .expect("write seed config");

        save_font_name_display_mode(&path, FontListKind::Folder, FontNameDisplayMode::Identity)
            .expect("save folder mode");

        let root = read_root(&path);
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|general| general.get("ui_language"))
                .and_then(Value::as_str),
            Some("ru")
        );
        let text_tab = root
            .get("TextTab")
            .and_then(Value::as_object)
            .expect("TextTab object");
        assert_eq!(
            text_tab.get("hanging_punctuation").and_then(Value::as_str),
            Some(",.")
        );
        assert_eq!(
            text_tab
                .get(FontListKind::Folder.config_key())
                .and_then(Value::as_str),
            Some(FontNameDisplayMode::Identity.as_config_str())
        );
    }
}

#[cfg(test)]
mod section_writer_tests {
    use super::*;
    use std::fs;

    #[test]
    fn typing_panel_layout_round_trips_and_preserves_other_keys() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");
        fs::write(&path, r#"{"General":{"ui_language":"ru"},"TextTab":{"x":1}}"#).expect("write seed config");

        save_typing_panel_layout(&path, TypingPanelLayout::Vertical).expect("save layout");
        assert_eq!(load_typing_panel_layout(&path), TypingPanelLayout::Vertical);

        let raw = fs::read_to_string(&path).expect("config written");
        let root: Value = serde_json::from_str(&raw).expect("valid json");
        assert_eq!(root["General"]["ui_language"], Value::String("ru".to_string()));
        assert_eq!(root["General"][GENERAL_TYPING_PANEL_LAYOUT_KEY], Value::String(TypingPanelLayout::Vertical.as_config_str().to_string()));
        assert_eq!(root["TextTab"]["x"], Value::from(1));
    }

    /// Declared behavior change: these writers used to degrade an unparsable file to `{}` and
    /// write it back, destroying every other setting. Now the error is reported and the file
    /// is left byte-for-byte unchanged.
    #[test]
    fn a_malformed_config_is_reported_and_left_untouched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");
        let malformed = r#"{"General":{"ui_language":"ru"},"TextTab":{"#;
        fs::write(&path, malformed).expect("write malformed config");

        save_typing_panel_layout(&path, TypingPanelLayout::Vertical).expect_err("layout writer must not overwrite");
        save_rotation_ctrl_wheel_mode(&path, crate::tabs::typing::rotation_ctrl_wheel::RotationCtrlWheelMode::Raster)
            .expect_err("rotation writer must not overwrite");
        save_font_name_display_mode(&path, FontListKind::Folder, FontNameDisplayMode::Identity).expect_err("name-mode writer must not overwrite");
        assert_eq!(fs::read_to_string(&path).expect("file still present"), malformed);
        // Readers fall back to their defaults.
        assert_eq!(load_typing_panel_layout(&path), TypingPanelLayout::Vertical);
    }
}

#[cfg(test)]
mod advanced_form_search_save_tests {
    use super::*;
    use std::fs;
    use crate::tabs::typing::advanced_form_params::AdvancedFormParams;

    /// Reads back a written config file as JSON.
    fn read_root(path: &Path) -> Value {
        let raw = fs::read_to_string(path).expect("config file written");
        serde_json::from_str::<Value>(&raw).expect("config file is valid json")
    }

    /// The knobs of a hand-picked, non-default setting, so the assertions cannot pass
    /// on the defaults by accident.
    fn tuned_params() -> AdvancedFormParams {
        AdvancedFormParams {
            per_bucket: 7,
            narrow_slots: 3,
            filters_prune: false,
            ..AdvancedFormParams::default()
        }
    }

    #[test]
    fn saving_the_knobs_preserves_every_unrelated_key() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");
        fs::write(
            &path,
            r#"{"General":{"ui_language":"ru"},"TextTab":{"hanging_punctuation":",."}}"#,
        )
        .expect("write seed config");

        config::save_advanced_form_search_params(&path, tuned_params().to_config_value())
            .expect("save the knobs");

        let root = read_root(&path);
        assert_eq!(
            root.get("General")
                .and_then(Value::as_object)
                .and_then(|general| general.get("ui_language"))
                .and_then(Value::as_str),
            Some("ru")
        );
        let text_tab = root
            .get("TextTab")
            .and_then(Value::as_object)
            .expect("TextTab object");
        assert_eq!(
            text_tab.get("hanging_punctuation").and_then(Value::as_str),
            Some(",."),
            "a sibling TextTab key must survive the write"
        );
        let stored = text_tab
            .get(config::TEXT_TAB_ADVANCED_FORM_SEARCH_KEY)
            .expect("the knobs object");
        assert_eq!(
            AdvancedFormParams::from_config_value(stored),
            tuned_params(),
            "the object must round-trip through the file"
        );
    }

    /// A malformed `user_config.json` must be REPORTED, never replaced: the previous
    /// recipe degraded an unparsable root to an empty object and then wrote it back,
    /// destroying every unrelated setting (and the ORT load-guard marker) because one
    /// knob moved.
    #[test]
    fn a_malformed_config_is_reported_and_left_untouched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("user_config.json");
        let malformed = r#"{"General":{"ui_language":"ru"},"TextTab":{"#;
        fs::write(&path, malformed).expect("write malformed config");

        let error =
            config::save_advanced_form_search_params(&path, tuned_params().to_config_value())
                .expect_err("a malformed config must not be overwritten");
        assert!(
            error.contains("parse"),
            "the error must name the parse failure, got: {error}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("the file must still be there"),
            malformed,
            "not one byte of the user's file may change"
        );
    }
}

#[cfg(test)]
mod spellcheck_words_save_tests {
    use super::*;

    fn request(custom: Option<&str>, project: Option<&str>) -> CanvasSettingsSaveRequest {
        CanvasSettingsSaveRequest {
            snapshot: SharedCanvasSettings::default(),
            comic_type: ComicType::from_canvas_preset_fields("none", false),
            custom_spellcheck_words: custom.map(str::to_owned),
            project_spellcheck_words: project.map(str::to_owned),
        }
    }

    /// The race this guards against: the editor loaded "alpha", the spellcheck context menu
    /// then persisted "beta" through its own worker, and an unrelated canvas-settings save
    /// follows before the editor refreshed. The unchanged editor text must not be sent, so
    /// the added word survives on disk.
    #[test]
    fn an_unchanged_editor_sends_no_word_list() {
        let mut synced = Some("alpha".to_owned());
        assert_eq!(words_to_send("alpha", &mut synced), None);
        assert_eq!(synced.as_deref(), Some("alpha"));
    }

    #[test]
    fn an_edited_list_is_sent_once_and_becomes_the_baseline() {
        let mut synced = Some("alpha".to_owned());
        assert_eq!(words_to_send("alpha\ngamma", &mut synced).as_deref(), Some("alpha\ngamma"));
        assert_eq!(synced.as_deref(), Some("alpha\ngamma"));
        assert_eq!(words_to_send("alpha\ngamma", &mut synced), None, "a second save of the same text is not re-sent");
    }

    #[test]
    fn coalescing_keeps_a_pending_edit_the_newer_request_does_not_carry() {
        let merged = request(Some("shared edit"), None).absorb_newer(request(None, Some("project edit")));
        assert_eq!(merged.custom_spellcheck_words.as_deref(), Some("shared edit"));
        assert_eq!(merged.project_spellcheck_words.as_deref(), Some("project edit"));
        let newest = request(Some("old"), None).absorb_newer(request(Some("new"), None));
        assert_eq!(newest.custom_spellcheck_words.as_deref(), Some("new"));
    }

    /// A request queued together with the shutdown signal (persist, then drop) must still be
    /// written: the shutdown is honored only after the coalesced save.
    #[test]
    fn a_shutdown_queued_behind_saves_writes_the_coalesced_request_first() {
        let (tx, rx) = mpsc::channel();
        for request in [request(Some("first"), None), request(None, Some("project")), request(Some("last"), None)] {
            tx.send(Some(request)).expect("receiver alive");
        }
        tx.send(None).expect("receiver alive");
        // Anything after the stop signal belongs to no one and is not saved.
        tx.send(Some(request(Some("after stop"), None))).expect("receiver alive");
        let mut saved = Vec::new();
        run_canvas_settings_saver(&rx, |latest| saved.push((latest.custom_spellcheck_words.clone(), latest.project_spellcheck_words.clone())));
        assert_eq!(saved, vec![(Some("last".to_owned()), Some("project".to_owned()))]);
    }

    #[test]
    fn a_failed_write_makes_the_next_save_resend_the_list() {
        let mut synced = Some("alpha".to_owned());
        assert_eq!(words_to_send("alpha\nbeta", &mut synced).as_deref(), Some("alpha\nbeta"));
        // The saver reports that "alpha\nbeta" could not be written.
        remark_unsynced_after_failure(&mut synced, "alpha\nbeta");
        assert_eq!(words_to_send("alpha\nbeta", &mut synced).as_deref(), Some("alpha\nbeta"), "the unchanged list is retried");
        assert_eq!(words_to_send("alpha\nbeta", &mut synced), None, "and not re-sent once more");
        // A failure of an OLDER text does not disturb a newer baseline.
        remark_unsynced_after_failure(&mut synced, "alpha");
        assert_eq!(synced.as_deref(), Some("alpha\nbeta"));
    }
}

