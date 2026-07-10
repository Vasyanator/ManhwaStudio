/*
File: src/launcher/pages/base.rs

Purpose:
Shared animated page runtime for launcher subpages.

Main responsibilities:
- define slide/fade transition state between fullscreen launcher pages;
- build clipped page layers positioned relative to the viewport;
- render shared page chrome, including the top-left "Назад" button.

Notes:
Transitions are non-blocking and progress off frame time, so the launcher UI stays responsive.
*/

use crate::config;
use crate::launcher::state::{LauncherPage, OpenProjectSelection};
use crate::launcher::theme;
use egui::{Align, Layout, Rect, Ui, UiBuilder};
use std::path::PathBuf;
use web_time::{Duration, Instant};

const TRANSITION_DURATION: Duration = Duration::from_millis(320);
const PAGE_MARGIN: f32 = 24.0;
const PAGE_TOP_PADDING: f32 = 78.0;
const BACK_BUTTON_SIZE: egui::Vec2 = egui::vec2(92.0, 40.0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageNavAction {
    Open(LauncherPage),
    OpenNewProjectWindow,
    BackToMain,
    OpenProject(OpenProjectSelection),
    StartUpdate,
    ProjectsRootChanged(PathBuf),
    AiInstallTypeChanged(config::AiInstallType),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy)]
pub struct PageLayer {
    pub page: LauncherPage,
    pub offset_x: f32,
    pub opacity: f32,
}

impl PageLayer {
    pub fn stationary(page: LauncherPage) -> Self {
        Self {
            page,
            offset_x: 0.0,
            opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PageTransition {
    from: LauncherPage,
    to: LauncherPage,
    started_at: Instant,
    direction: TransitionDirection,
}

impl PageTransition {
    pub fn new(from: LauncherPage, to: LauncherPage) -> Self {
        let direction = if matches!(to, LauncherPage::Main) {
            TransitionDirection::Backward
        } else {
            TransitionDirection::Forward
        };
        Self {
            from,
            to,
            started_at: Instant::now(),
            direction,
        }
    }

    pub fn target(&self) -> LauncherPage {
        self.to
    }

    pub fn is_finished(&self) -> bool {
        self.started_at.elapsed() >= TRANSITION_DURATION
    }

    pub fn visible_layers(&self, viewport_width: f32) -> [PageLayer; 2] {
        let progress = eased_progress(
            self.started_at.elapsed().as_secs_f32() / TRANSITION_DURATION.as_secs_f32(),
        );
        let fade_out = (1.0 - progress * 0.7).clamp(0.0, 1.0);
        let fade_in = progress.clamp(0.0, 1.0);

        match self.direction {
            TransitionDirection::Forward => [
                PageLayer {
                    page: self.from,
                    offset_x: -viewport_width * progress,
                    opacity: fade_out,
                },
                PageLayer {
                    page: self.to,
                    offset_x: viewport_width * (1.0 - progress),
                    opacity: fade_in,
                },
            ],
            TransitionDirection::Backward => [
                PageLayer {
                    page: self.from,
                    offset_x: viewport_width * progress,
                    opacity: fade_out,
                },
                PageLayer {
                    page: self.to,
                    offset_x: -viewport_width * (1.0 - progress),
                    opacity: fade_in,
                },
            ],
        }
    }
}

pub fn make_layer_ui(ui: &mut Ui, viewport_rect: Rect, layer: PageLayer) -> Ui {
    let page_rect = viewport_rect.translate(egui::vec2(layer.offset_x, 0.0));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(page_rect)
            .layout(Layout::top_down(Align::Center)),
    );
    child.set_clip_rect(viewport_rect);
    child.set_opacity(layer.opacity);
    child
}

pub fn show_page_shell(ui: &mut Ui, add_body: impl FnOnce(&mut Ui)) -> Option<PageNavAction> {
    let rect = ui.max_rect();
    let back_rect = Rect::from_min_size(
        egui::pos2(rect.left() + PAGE_MARGIN, rect.top() + PAGE_MARGIN),
        BACK_BUTTON_SIZE,
    );
    let mut back_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(back_rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    back_ui.set_clip_rect(rect);
    let back_clicked =
        theme::launcher_button(&mut back_ui, t!("launcher.page.back_button"), BACK_BUTTON_SIZE, true).clicked();

    let body_rect = Rect::from_min_max(
        egui::pos2(rect.left() + PAGE_MARGIN, rect.top() + PAGE_TOP_PADDING),
        egui::pos2(rect.right() - PAGE_MARGIN, rect.bottom() - PAGE_MARGIN),
    );
    let mut body_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(body_rect)
            .layout(Layout::top_down(Align::Center)),
    );
    body_ui.set_clip_rect(rect);
    add_body(&mut body_ui);

    back_clicked.then_some(PageNavAction::BackToMain)
}

fn eased_progress(progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    1.0 - (1.0 - progress).powi(3)
}
