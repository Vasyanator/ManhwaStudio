/*
File: src/studio/tabs.rs

Purpose:
The studio's ten program-tab states plus the Translation tab's canvas, as one owned struct
(`Tabs`), and the per-frame draw of the active tab. A tab never reaches `StudioApp`: what it needs
for one frame comes in as a borrowed `TabFrame`, and what it asks of the app goes out as
`AppRequest`s that `StudioApp` applies right after the draw.

Key structures:
- `Tabs`: the tab states (owned by `StudioApp`, field `tabs`).
- `TabFrame`: the borrowed frame context of one draw (project, page geometry + resident textures
  lent by `PageResidency`, panel dock, busy snapshot, canvas status, hotkey registry).
- `AppRequest`: every tab -> app request of one draw (page op, page open, settings deep link,
  characters / terms changed, open notes for a character).

Key functions:
- `Tabs::draw`: the per-tab draw dispatch of `StudioApp::ui`.
- `Tabs::view_tabs` / `Tabs::canvas`: the borrows `ViewSync` and the per-canvas fan-outs use
  (the tab -> canvas mapping stays `view_sync::canvas_slot`).
- `Tabs::apply_typing_panel_layout_request`: the settings -> typing panel-layout hand-off.
- `build_translation_hint_rows` / `build_typing_hint_rows`: the canvas bottom-hint rows.

Notes:
`Tabs::draw` never changes `StudioApp::active_tab`: a switch is a request, applied by the app after
the draw (and after the panel-layout poll), at the point today's arms used to switch.
*/

use super::busy::PersistenceActivity;
use super::view_sync::{ViewTabs, canvas_of};
use crate::canvas::{
    CanvasBottomHint, CanvasDrawParams, CanvasHintHelp, CanvasHintRow, CanvasUiStatus, CanvasView,
    SourceTextureUploadBudget,
};
use crate::input_manager_v2::InputManagerV2;
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
use crate::tabs::translation::TranslationTabState;
use crate::tabs::typing::{TypingDrawParams, TypingTabState};
use crate::tabs::wiki::WikiTabState;
use crate::widgets::panel_dock::PanelDockState;
use eframe::egui;
use std::collections::HashMap;

/// The studio's program-tab states (owned by `StudioApp`). Field order is the drop order of the
/// tab states (the Translation canvas first), which is the order they had as `StudioApp` fields.
pub(super) struct Tabs {
    /// The Translation tab's canvas (the cleaning and typing tabs own theirs).
    pub(super) translation_canvas: CanvasView,
    pub(super) translation: TranslationTabState,
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
    /// The hotkey registry, edited by the Settings tab.
    pub(super) hotkeys: &'a mut InputManagerV2,
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
    /// The view tabs `ViewSync` drives, borrowed for one call.
    pub(super) fn view_tabs(&mut self) -> ViewTabs<'_> {
        ViewTabs { translation: &mut self.translation_canvas, cleaning: &mut self.cleaning, typing: &mut self.typing, ps_editor: &mut self.ps_editor }
    }

    /// The canvas `tab` draws with, if any (the mapping is `view_sync::canvas_slot`).
    pub(super) fn canvas(&self, tab: AppTab) -> Option<&CanvasView> {
        canvas_of(tab, &self.translation_canvas, &self.cleaning, &self.typing)
    }

    /// Hands a panel layout the Settings tab picked to the typing tab. Called at the start of every
    /// frame (`StudioApp::new` does the same hand-off for the seeded layout before `Tabs` exists).
    pub(super) fn apply_typing_panel_layout_request(&mut self) {
        if let Some(layout) = self.settings.take_typing_panel_layout_request() {
            self.typing.set_panel_layout(layout);
        }
    }

    /// Draws `drawn` (the active tab) into a central panel of `ui` and returns what it asks of the
    /// app, in the order raised. Never switches tabs itself and never polls the panel dock's dirty
    /// layouts (the app does both after the draw).
    pub(super) fn draw(&mut self, drawn: AppTab, ui: &mut egui::Ui, frame: TabFrame<'_>) -> Vec<AppRequest> {
        let TabFrame { ctx, project, page_infos, textures, status, overlays_loading, activity, panel_dock, hotkeys } = frame;
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
                self.translation_canvas.set_bottom_hint(Some(CanvasBottomHint {
                    rows: build_translation_hint_rows(),
                }));
                self.translation.sync_with_project_settings(project);
                self.translation.draw_side_panel(ctx, &mut self.translation_canvas, project, page_infos);
                let canvas = &mut self.translation_canvas;
                canvas.set_drag_scroll_blocked(false);
                canvas.set_wheel_scroll_blocked(false);
                canvas.set_zoom_blocked(self.translation.blocks_canvas_zoom());
                canvas.set_overlay_render_suppressed(false);
                canvas.set_overlay_upload_min_interval_s(0.0);
                let hooks = &mut self.translation;
                // `panel_dock` is lent for the frame and disjoint from `hooks` and `canvas`,
                // exactly as for «Текст»; the canvas only carries it through to the overlay hook,
                // where this tab runs its dock.
                egui::CentralPanel::default().show(ui, |ui| {
                    let mut source_upload_budget =
                        SourceTextureUploadBudget::source_page_reupload_default();
                    canvas.draw(CanvasDrawParams {
                        ctx,
                        ui,
                        project,
                        page_infos,
                        texture_cache: textures,
                        status,
                        source_upload_budget: &mut source_upload_budget,
                        hooks,
                        panel_dock,
                    });
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
                    });
                });
            }
            AppTab::Typing => {
                let typing = &mut self.typing;
                typing.set_bottom_hint(Some(CanvasBottomHint {
                    rows: build_typing_hint_rows(),
                }));
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
                egui::CentralPanel::default().show(ui, |ui| settings.draw(ui, hotkeys));
            }
            AppTab::Wiki => {
                let wiki = &mut self.wiki;
                egui::CentralPanel::default().show(ui, |ui| wiki.draw(ui));
            }
        }
        requests
    }
}

/// Builds the fixed, hand-authored Translation-tab bottom-hint rows.
///
/// Content is a curated set of localized label/key pairs (via `t!`), NOT sourced from the hotkey
/// registry. Rebuilt every frame so a runtime language switch re-localizes the rows. Order is
/// canonical and must match the documented Translation shortcut set. No row carries help, so this
/// hint stays two-column; attach one with `CanvasHintRow::with_help` when a shortcut needs it.
#[must_use]
fn build_translation_hint_rows() -> Vec<CanvasHintRow> {
    vec![
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.create_bubble_label"),
            t!("canvas.bottom_hint.translation.create_bubble_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.create_bubble_recent_char_label"),
            t!("canvas.bottom_hint.translation.create_bubble_recent_char_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.ocr_bubble_label"),
            t!("canvas.bottom_hint.translation.ocr_bubble_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.ocr_bubble_recent_char_label"),
            t!("canvas.bottom_hint.translation.ocr_bubble_recent_char_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.advanced_ocr_label"),
            t!("canvas.bottom_hint.translation.advanced_ocr_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.create_image_bubble_label"),
            t!("canvas.bottom_hint.translation.create_image_bubble_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.create_image_bubble_crop_label"),
            t!("canvas.bottom_hint.translation.create_image_bubble_crop_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.create_hint_bubble_label"),
            t!("canvas.bottom_hint.translation.create_hint_bubble_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.translation.delete_bubble_label"),
            t!("canvas.bottom_hint.translation.delete_bubble_keys"),
        ),
    ]
}

/// Builds the fixed, hand-authored Typing-tab bottom-hint rows.
///
/// Content is a curated set of localized label/key pairs (via `t!`), NOT sourced from the hotkey
/// registry. Rebuilt every frame so a runtime language switch re-localizes the rows. Order is
/// canonical and must match the documented Typing shortcut set. The select-text-area row carries an
/// animated "?" help icon, which puts this hint into the grid's three-column mode.
#[must_use]
fn build_typing_hint_rows() -> Vec<CanvasHintRow> {
    vec![
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.select_text_area_label"),
            t!("canvas.bottom_hint.typing.select_text_area_keys"),
        )
        .with_help(CanvasHintHelp::Animation(ms_gifs::typing::SELECT_TEXT_AREA)),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.quick_font_size_label"),
            t!("canvas.bottom_hint.typing.quick_font_size_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.rotate_label"),
            t!("canvas.bottom_hint.typing.rotate_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.scale_label"),
            t!("canvas.bottom_hint.typing.scale_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.nudge_label"),
            t!("canvas.bottom_hint.typing.nudge_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.toggle_clean_label"),
            t!("canvas.bottom_hint.typing.toggle_clean_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.toggle_centering_assist_label"),
            t!("canvas.bottom_hint.typing.toggle_centering_assist_keys"),
        ),
    ]
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
