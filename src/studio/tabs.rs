/*
File: src/studio/tabs.rs

Purpose:
The studio's ten program tabs as one owned struct (`Tabs`), the ONE `AppTab` -> tab mapping
(`Tabs::get` / `get_mut`, as `ms_studio_tab::StudioTab` trait objects) with the order-free hook
fan-outs built on it, and the per-frame draw of the active tab. A tab never reaches `StudioApp`:
what it needs for one frame comes in as a borrowed `TabFrame`, and what it asks of the app goes
out as `AppRequest`s that `StudioApp` applies right after the draw.

Key structures:
- `Tabs`: the tab states (owned by `StudioApp`, field `tabs`); the Translation tab is its host
  `TranslationTab`, which owns the Translation canvas.
- `TabFrame`: the borrowed frame context of one draw (project, page geometry + resident textures
  lent by `PageResidency`, panel dock, busy snapshot, canvas status, the input router: its
  frame and registry for the drawing tab, the router itself for the Settings pane).
- `AppRequest`: every tab -> app request of one draw (page op, page open, settings deep link,
  characters / terms changed, open notes for a character).

Key functions:
- `Tabs::draw`: the per-tab draw dispatch of `StudioApp::ui`.
- `Tabs::get` / `get_mut` / `for_each` / `for_each_mut`: the tab -> `dyn StudioTab` mapping and
  the fan-outs in `AppTab::ALL` order; `input_declarations`, `dock_layout_defaults`,
  `hosts_panel_dock`, `evict_tab_gpu_caches` are the fan-outs `studio.rs` reads, `write_exit_settings` and
  `flush_debounced_stores_blocking` the ones the teardown (`shutdown.rs`) calls.
- `Tabs::view_tabs`: the borrow `ViewSync` and the per-canvas fan-outs use (the tab -> canvas
  mapping stays `view_sync::canvas_slot`).

Notes:
- `Tabs::draw` never changes `StudioApp::active_tab`: a switch is a request, applied by the app
  after the draw (and after the panel-layout poll), at the point today's arms used to switch.
- Only ORDER-FREE questions go through the trait fan-outs; drawing and every ordered §12 sequence
  (flushes, staging writers, teardown) stay explicit calls on the concrete fields.
*/

use super::busy::PersistenceActivity;
use super::view_sync::ViewTabs;
use crate::canvas::CanvasUiStatus;
use crate::memory_manager::{CacheEvictionReport, CacheEvictionRequest};
use crate::page_ops::PageOpKind;
use crate::page_view::{PageImageInfo, PageTexture};
use crate::project::ProjectData;
use crate::settings_shared::SettingsDeepLink;
use crate::tabs::AppTab;
use crate::tabs::characters::{CharactersTabAction, CharactersTabState};
use crate::tabs::cleaning::{CleaningDrawParams, CleaningTabState};
use crate::tabs::notes::NotesTabState;
use crate::tabs::page_manager::{PageManagerAction, PageManagerTabState};
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::settings::SettingsTabState;
use crate::tabs::terms::TermsTabState;
use crate::tabs::translation::{TranslationDrawParams, TranslationTab};
use crate::tabs::typing::{TypingDrawParams, TypingTabState};
use crate::tabs::wiki::WikiTabState;
use crate::widgets::panel_dock::PanelDockState;
use crate::widgets::panel_dock::persist::LayoutDefault;
use eframe::egui;
use ms_input::{Declarations, InputRouter};
use ms_studio_tab::StudioTab;
use std::collections::HashMap;

/// The studio's program-tab states (owned by `StudioApp`). Field order is the drop order of the
/// tab states (the Translation canvas — first field of `TranslationTab` — first), which is the
/// order they had as `StudioApp` fields.
pub(super) struct Tabs {
    /// The Translation tab: its canvas plus its state (the cleaning and typing tabs own their
    /// canvases the same way).
    pub(super) translation: TranslationTab,
    pub(super) cleaning: CleaningTabState,
    pub(super) typing: TypingTabState,
    pub(super) ps_editor: PsEditorTabState,
    pub(super) page_manager: PageManagerTabState,
    pub(super) characters: CharactersTabState,
    pub(super) terms: TermsTabState,
    pub(super) notes: NotesTabState,
    pub(super) settings: SettingsTabState,
    pub(super) wiki: WikiTabState,
}

/// What the drawing tab gets for one frame, borrowed from `StudioApp` (disjoint from `Tabs`).
pub(super) struct TabFrame<'a> {
    pub(super) ctx: &'a egui::Context,
    pub(super) project: &'a ProjectData,
    /// Source-page geometry and load state, lent by `PageResidency::lend_mut`.
    pub(super) page_infos: &'a HashMap<usize, PageImageInfo>,
    /// The resident source textures, lent with `page_infos` (a tab may re-upload into them).
    pub(super) textures: &'a mut HashMap<usize, PageTexture>,
    /// Page load counters shown by the three canvas tabs.
    pub(super) status: CanvasUiStatus,
    /// Whether the clean overlays are still being delivered (page manager clean mutations wait).
    pub(super) overlays_loading: bool,
    /// The persistence busy snapshot of this frame, taken before the tab states were borrowed.
    pub(super) activity: PersistenceActivity,
    /// The studio window's panel dock, lent to the dock hosts.
    pub(super) panel_dock: &'a mut PanelDockState,
    /// The input router after this frame's `begin_frame`: the drawing tab reads its frame and
    /// registry, the Settings tab edits its bindings (applied from the next frame).
    pub(super) input: &'a mut InputRouter,
}

/// A tab's request to the app, returned by [`Tabs::draw`] in the order the tab raised it and
/// applied by `StudioApp::apply_app_request` right after the draw.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AppRequest {
    /// Run a structural page operation (page manager).
    PageOp(PageOpKind),
    /// Switch to `tab` and focus page `page_idx` there (page manager).
    OpenPage { tab: AppTab, page_idx: usize },
    /// Reveal a settings block and switch to the Settings tab (typing panel's font-group help).
    NavigateSettings(SettingsDeepLink),
    /// The character list changed: the Notes and Translation tabs reload it.
    CharactersChanged,
    /// Open the Notes tab with this character as context (the character list is reloaded too).
    OpenNotesForCharacter(String),
    /// The term list changed: the Notes tab reloads it.
    TermsChanged,
}

impl AppRequest {
    /// The request a page-manager action becomes (1:1).
    #[must_use]
    fn from_page_manager(action: PageManagerAction) -> Self {
        match action {
            PageManagerAction::RequestOp(op) => Self::PageOp(op),
            PageManagerAction::OpenPageIn { tab, page_idx } => Self::OpenPage { tab, page_idx },
        }
    }

    /// The request a characters-tab action becomes (1:1).
    #[must_use]
    fn from_characters(action: CharactersTabAction) -> Self {
        match action {
            CharactersTabAction::CharactersChanged => Self::CharactersChanged,
            CharactersTabAction::OpenNotesForCharacter(name) => Self::OpenNotesForCharacter(name),
        }
    }
}

impl Tabs {
    /// The tab state `tab` names, as its order-free hook surface. The ONE `AppTab` -> tab mapping
    /// of `src/studio` (exhaustive: a new tab must answer it).
    pub(super) fn get(&self, tab: AppTab) -> &dyn StudioTab {
        match tab {
            AppTab::PageManager => &self.page_manager,
            AppTab::Translation => &self.translation,
            AppTab::Cleaning => &self.cleaning,
            AppTab::Typing => &self.typing,
            AppTab::PsEditor => &self.ps_editor,
            AppTab::Characters => &self.characters,
            AppTab::Terms => &self.terms,
            AppTab::Notes => &self.notes,
            AppTab::Settings => &self.settings,
            AppTab::Wiki => &self.wiki,
        }
    }

    /// [`Self::get`], mutably.
    pub(super) fn get_mut(&mut self, tab: AppTab) -> &mut dyn StudioTab {
        match tab {
            AppTab::PageManager => &mut self.page_manager,
            AppTab::Translation => &mut self.translation,
            AppTab::Cleaning => &mut self.cleaning,
            AppTab::Typing => &mut self.typing,
            AppTab::PsEditor => &mut self.ps_editor,
            AppTab::Characters => &mut self.characters,
            AppTab::Terms => &mut self.terms,
            AppTab::Notes => &mut self.notes,
            AppTab::Settings => &mut self.settings,
            AppTab::Wiki => &mut self.wiki,
        }
    }

    /// Calls `f` for every tab, in `AppTab::ALL` order. Only for order-free hooks (the trait's
    /// contract): no caller may depend on which tab is visited first.
    pub(super) fn for_each_mut(&mut self, mut f: impl FnMut(&mut dyn StudioTab)) {
        for tab in AppTab::ALL {
            f(self.get_mut(tab));
        }
    }

    /// Calls `f` for every tab, in `AppTab::ALL` order (shared borrow). Same contract as
    /// [`Self::for_each_mut`]: order-free hooks only.
    pub(super) fn for_each(&self, mut f: impl FnMut(&dyn StudioTab)) {
        for tab in AppTab::ALL {
            f(self.get(tab));
        }
    }

    /// Writes every tab's exit-only `user_config` keys into `root` (pure; the caller runs it inside
    /// its ONE `update_user_config_file`, `shutdown.rs`). Tabs write disjoint keys, so the visiting
    /// order does not change the result.
    pub(super) fn write_exit_settings(&self, root: &mut serde_json::Value) {
        self.for_each(|tab| tab.write_exit_settings(root));
    }

    /// Teardown only: synchronously flushes every tab's debounced user-global stores (each tab
    /// flushes its own stores in its own order and logs what it wrote). Blocking.
    pub(super) fn flush_debounced_stores_blocking(&mut self) {
        self.for_each_mut(|tab| tab.flush_debounced_stores_blocking());
    }

    /// Every tab's input declarations merged, tabs in `AppTab::ALL` order, each tab's own
    /// declaration order kept (pinned by `input_registry_golden`).
    #[must_use]
    pub(super) fn input_declarations(&self) -> Declarations {
        let mut merged = Declarations::default();
        self.for_each(|tab| merged.extend(tab.input_declarations()));
        merged
    }

    /// The default-layout builder of every tab that hosts the panel dock, keyed by
    /// `AppTab::key()` — the dictionary `panel_dock::persist` restores the stored arrangement
    /// against. Each builder is the SAME `fn` item its tab hands to `ensure_default_layout`
    /// (pinned by the tab crates' tests), so «сбросить раскладку» resets to what the user saw.
    #[must_use]
    pub(super) fn dock_layout_defaults(&self) -> Vec<LayoutDefault<'static>> {
        AppTab::ALL.into_iter().filter_map(|tab| self.get(tab).default_dock_layout().map(|builder| (tab.key(), builder))).collect()
    }

    /// Whether `tab` draws the app-owned panel dock (`PanelDock::end` runs while it is active).
    /// The same answer as [`Self::dock_layout_defaults`], from the same hook.
    #[must_use]
    pub(super) fn hosts_panel_dock(&self, tab: AppTab) -> bool {
        self.get(tab).default_dock_layout().is_some()
    }

    /// Frees every tab's own GPU caches under `request` (in `AppTab::ALL` order; the request is
    /// immutable, so the order does not change what is freed) and returns the merged report.
    pub(super) fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = CacheEvictionReport::default();
        self.for_each_mut(|tab| report.append(tab.evict_tab_gpu_caches(request)));
        report
    }

    /// The view tabs `ViewSync` drives, borrowed for one call.
    pub(super) fn view_tabs(&mut self) -> ViewTabs<'_> {
        ViewTabs { translation: self.translation.canvas_mut(), cleaning: &mut self.cleaning, typing: &mut self.typing, ps_editor: &mut self.ps_editor }
    }

    /// Draws `drawn` (the active tab) into a central panel of `ui` and returns what it asks of the
    /// app, in the order raised. Never switches tabs itself and never polls the panel dock's dirty
    /// layouts (the app does both after the draw).
    pub(super) fn draw(&mut self, drawn: AppTab, ui: &mut egui::Ui, frame: TabFrame<'_>) -> Vec<AppRequest> {
        let TabFrame { ctx, project, page_infos, textures, status, overlays_loading, activity, panel_dock, input } = frame;
        let mut requests = Vec::new();
        match drawn {
            AppTab::PageManager => {
                let op_in_progress = activity.blocks_page_op();
                let tab = &mut self.page_manager;
                // Clean mutations stay refused until every overlay has been delivered: a late
                // `load_prepared_overlay` would overwrite what a bind/unlink put into the model.
                tab.set_overlays_loading(overlays_loading);
                let mut actions = Vec::new();
                // `page_infos` / `textures` are lent for the frame like for the canvas tabs: the
                // page viewer draws a page from the resident tiles instead of decoding it again.
                egui::CentralPanel::default().show(ui, |ui| {
                    actions = tab.draw(ctx, ui, project, page_infos, textures, op_in_progress);
                });
                requests.extend(actions.into_iter().map(AppRequest::from_page_manager));
            }
            AppTab::Translation => {
                // The host sets its hint rows, runs its side panel and opens its own central
                // panel (`TranslationTab::draw`); the dock runs inside the canvas overlay hook.
                self.translation.draw(TranslationDrawParams {
                    ctx,
                    ui,
                    project,
                    page_infos,
                    texture_cache: textures,
                    status,
                    panel_dock,
                    input: input.frame(),
                    input_registry: input.registry(),
                });
            }
            AppTab::Cleaning => {
                // The Cleaning tab has no bottom-hint: its canvas `bottom_hint` stays `None`, so
                // `draw_canvas_bottom_hint` early-returns and nothing is drawn.
                let cleaning = &mut self.cleaning;
                egui::CentralPanel::default().show(ui, |ui| {
                    cleaning.draw(CleaningDrawParams {
                        ctx,
                        ui,
                        project,
                        page_infos,
                        texture_cache: textures,
                        status,
                        panel_dock,
                        input: input.frame(),
                        input_registry: input.registry(),
                    });
                });
            }
            AppTab::Typing => {
                // The typing tab sets its own bottom-hint rows at the start of its draw.
                let typing = &mut self.typing;
                // Export is mutually exclusive with a project save (Finding 2): the save may be pending
                // on the whole-project preload or already running. Push it into the typing tab before
                // draw so both the export trigger and the deferred-export gate can suppress dispatch.
                typing.set_save_busy(activity.save_busy());
                // The dock state is app-owned and lent to the tab for the frame; the borrow is
                // disjoint from `typing`, which is what lets the tab split it against its own
                // fields (`dev-docs/dockable_panels_plan.md` §4.2).
                egui::CentralPanel::default().show(ui, |ui| {
                    typing.draw(TypingDrawParams {
                        ctx,
                        ui,
                        project,
                        page_infos,
                        texture_cache: textures,
                        status,
                        panel_dock,
                        input: input.frame(),
                        input_registry: input.registry(),
                    });
                });
                // In-app deep link from the typing panel's font-group "?" help icon: the app
                // reveals the requested block and switches to the settings tab.
                if let Some(link) = typing.take_settings_navigation_request() {
                    requests.push(AppRequest::NavigateSettings(link));
                }
            }
            AppTab::PsEditor => {
                let ps_editor = &mut self.ps_editor;
                // The dock state is app-owned and lent to the tab for the frame; the borrow is
                // disjoint from `ps_editor`, which is what lets the tab hand `&mut Self` to its own
                // dock bodies (`dev-docs/dockable_panels_plan.md` §4.2).
                ps_editor.handle_hotkeys(ctx, project);
                egui::CentralPanel::default().show(ui, |ui| {
                    ps_editor.draw(ctx, ui, project, panel_dock);
                });
            }
            AppTab::Characters => {
                let tab = &mut self.characters;
                let mut actions = Vec::new();
                egui::CentralPanel::default().show(ui, |ui| {
                    actions = tab.draw(ctx, ui, project);
                });
                requests.extend(actions.into_iter().map(AppRequest::from_characters));
            }
            AppTab::Terms => {
                let tab = &mut self.terms;
                let mut changed = false;
                egui::CentralPanel::default().show(ui, |ui| {
                    changed = tab.draw(ctx, ui, project);
                });
                if changed {
                    requests.push(AppRequest::TermsChanged);
                }
            }
            AppTab::Notes => {
                let tab = &mut self.notes;
                egui::CentralPanel::default().show(ui, |ui| tab.draw(ctx, ui, project));
            }
            AppTab::Settings => {
                let settings = &mut self.settings;
                egui::CentralPanel::default().show(ui, |ui| settings.draw(ui, input));
            }
            AppTab::Wiki => {
                let wiki = &mut self.wiki;
                egui::CentralPanel::default().show(ui, |ui| wiki.draw(ui));
            }
        }
        requests
    }
}

#[cfg(test)]
mod tests {
    use super::AppRequest;
    use crate::page_ops::PageOpKind;
    use crate::tabs::AppTab;
    use crate::tabs::characters::CharactersTabAction;
    use crate::tabs::page_manager::PageManagerAction;

    /// A tab's actions become requests one to one and in the order the tab raised them, so the
    /// app applies a page op and a page open (or a characters change and an «open notes») in
    /// exactly the order the old in-arm loops did.
    #[test]
    fn tab_actions_become_requests_in_order() {
        let page_manager = [
            PageManagerAction::OpenPageIn { tab: AppTab::Cleaning, page_idx: 2 },
            PageManagerAction::RequestOp(PageOpKind::Move { from: 0, to: 1 }),
            PageManagerAction::OpenPageIn { tab: AppTab::Typing, page_idx: 5 },
        ];
        let requests: Vec<AppRequest> = page_manager.into_iter().map(AppRequest::from_page_manager).collect();
        assert_eq!(
            requests,
            vec![
                AppRequest::OpenPage { tab: AppTab::Cleaning, page_idx: 2 },
                AppRequest::PageOp(PageOpKind::Move { from: 0, to: 1 }),
                AppRequest::OpenPage { tab: AppTab::Typing, page_idx: 5 },
            ]
        );

        let characters = [
            CharactersTabAction::OpenNotesForCharacter("Mira".to_string()),
            CharactersTabAction::CharactersChanged,
        ];
        let requests: Vec<AppRequest> = characters.into_iter().map(AppRequest::from_characters).collect();
        assert_eq!(requests, vec![AppRequest::OpenNotesForCharacter("Mira".to_string()), AppRequest::CharactersChanged]);
    }
}
