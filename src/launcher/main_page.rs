/*
File: src/launcher/main_page.rs

Purpose:
Main page renderer for the Rust launcher menu screen.

Main responsibilities:
- mirror the Python launcher's central menu card;
- keep the button grid and footer layout isolated from runtime logic;
- show installer-mode notices from `General.ai_install_type` under the main menu;
- render the central UI card on top of the blur layer with the same button/status composition as launcher.py.
*/

use crate::config;
use crate::launcher::app::LauncherApp;
use crate::launcher::pages::base::PageNavAction;
use crate::launcher::state::LauncherPage;
use crate::launcher::theme;
#[cfg(feature = "tutorial")]
use crate::launcher::tutorial;
use egui::{Align, Area, Color32, Frame, Grid, Layout, Order, RichText, Stroke, Ui, Vec2};

const LEFT_COLUMN_BUTTON_WIDTH: f32 = 210.0;
const RIGHT_COLUMN_BUTTON_WIDTH: f32 = 190.0;
const BUTTON_HEIGHT: f32 = 42.0;
const MENU_BLOCK_LEFT_OFFSET: f32 = 12.0;
const IMPORT_POPUP_GAP: f32 = 10.0;
const IMPORT_POPUP_WIDTH: f32 = 178.0;
const UPDATE_NOTICE_WIDTH: f32 = 310.0;
const UPDATE_NOTICE_OUTER_WIDTH: f32 = UPDATE_NOTICE_WIDTH + 36.0;
const UPDATE_NOTICE_OUTER_HEIGHT: f32 = 126.0;
const UPDATE_NOTICE_GAP: f32 = 18.0;
const UPDATE_NOTICE_TOP_MARGIN: f32 = 22.0;
const AI_INSTALL_NOTICE_WIDTH: f32 = 460.0;
const AI_INSTALL_NOTICE_MAX_HEIGHT: f32 = 96.0;

pub fn show(app: &mut LauncherApp, ui: &mut Ui) -> Option<PageNavAction> {
    let mut action = None;
    let mut import_button_rect = None;
    let viewport = ui.max_rect();
    let menu_top_space = menu_top_space(viewport.height(), app.update_notification.is_some());
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add_space(menu_top_space);

        theme::card_frame().show(ui, |ui| {
            ui.set_width(460.0);
            ui.vertical_centered(|ui| {
                #[cfg(not(target_arch = "wasm32"))]
                ui.label(theme::hero_title("ManhwaStudio"));
                #[cfg(target_arch = "wasm32")]
                ui.label(theme::hero_title(t!("launcher.main.web_demo_title")));
                ui.add_space(8.0);

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(MENU_BLOCK_LEFT_OFFSET);
                    Grid::new("launcher_menu_grid")
                        .num_columns(2)
                        .spacing([18.0, 12.0])
                        .show(ui, |ui| {
                            // Each menu button records its rect for the tutorial
                            // overlay before its click is consumed (keys must
                            // match `launcher::tutorial`).
                            let open_response = menu_button_response(
                                ui,
                                t!("launcher.main.open_chapter_button"),
                                LEFT_COLUMN_BUTTON_WIDTH,
                            );
                                            #[cfg(feature = "tutorial")]
                            app.tutorial.mark(tutorial::TARGET_OPEN, open_response.rect);
                            if open_response.clicked() {
                                app.state.import_popup_open = false;
                                action = Some(PageNavAction::Open(LauncherPage::OpenProject));
                            }
                            let new_response = menu_button_response(
                                ui,
                                t!("launcher.main.new_chapter_button"),
                                RIGHT_COLUMN_BUTTON_WIDTH,
                            );
                            #[cfg(feature = "tutorial")]
                            app.tutorial.mark(tutorial::TARGET_NEW, new_response.rect);
                            if new_response.clicked() {
                                app.state.import_popup_open = false;
                                action = Some(PageNavAction::OpenNewProjectWindow);
                            }
                            ui.end_row();

                            let import_response = menu_button_response(
                                ui,
                                t!("launcher.main.import_chapter_button"),
                                LEFT_COLUMN_BUTTON_WIDTH,
                            );
                            import_button_rect = Some(import_response.rect);
                            #[cfg(feature = "tutorial")]
                            app.tutorial.mark(tutorial::TARGET_IMPORT, import_response.rect);
                            if import_response.clicked() {
                                app.state.main_page_message = None;
                                app.state.import_popup_open = !app.state.import_popup_open;
                            }
                            let export_response = menu_button_response(
                                ui,
                                t!("launcher.main.export_chapter_button"),
                                RIGHT_COLUMN_BUTTON_WIDTH,
                            );
                            #[cfg(feature = "tutorial")]
                            app.tutorial.mark(tutorial::TARGET_EXPORT, export_response.rect);
                            if export_response.clicked() {
                                app.state.import_popup_open = false;
                                app.state.main_page_message = None;
                                action = Some(PageNavAction::Open(LauncherPage::ExportChapter));
                            }
                            ui.end_row();

                            if let Some(message) = app.state.main_page_message.as_deref() {
                                ui.colored_label(theme::TEXT_MUTED, message);
                                ui.label("");
                                ui.end_row();
                            }
                        });
                });
                ui.add_space(12.0);
                let settings_response =
                    menu_button_response(ui, t!("launcher.main.settings_button"), RIGHT_COLUMN_BUTTON_WIDTH);
                #[cfg(feature = "tutorial")]
                app.tutorial.mark(tutorial::TARGET_SETTINGS, settings_response.rect);
                if settings_response.clicked() {
                    app.state.import_popup_open = false;
                    action = Some(PageNavAction::Open(LauncherPage::Settings));
                }
            });
        });

        if let Some(notice) = ai_install_notice(app.ai_install_type) {
            ui.add_space(14.0);
            show_ai_install_notice(ui, notice);
        }

        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.add_space(24.0);
            ui.label(theme::footer(&app.state.footer_label));
        });
    });
    if let Some(update_action) = show_update_notice(app, ui, menu_top_space) {
        action = Some(update_action);
    }
    if let Some(import_action) = show_import_popup(app, ui, import_button_rect) {
        action = Some(import_action);
    }
    action
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AiInstallNotice {
    message: &'static str,
    fill: Color32,
    stroke: Color32,
    text: Color32,
}

fn ai_install_notice(install_type: config::AiInstallType) -> Option<AiInstallNotice> {
    // Web (wasm) build: a dedicated "Веб-версия" notice replaces the desktop
    // install-state notices (the AI/install concept does not apply on the web).
    #[cfg(target_arch = "wasm32")]
    {
        let _ = install_type;
        Some(AiInstallNotice {
            message: t!("launcher.main.web_demo_notice"),
            fill: Color32::from_rgba_premultiplied(22, 42, 72, 152),
            stroke: Color32::from_rgba_premultiplied(96, 152, 224, 168),
            text: Color32::from_rgb(206, 226, 255),
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        match install_type {
            config::AiInstallType::None => Some(AiInstallNotice {
                message: t!("launcher.main.offline_mode_notice"),
                fill: Color32::from_rgba_premultiplied(96, 18, 22, 150),
                stroke: Color32::from_rgba_premultiplied(238, 96, 104, 170),
                text: Color32::from_rgb(255, 218, 220),
            }),
            config::AiInstallType::Base => Some(AiInstallNotice {
                message: t!("launcher.main.lite_version_notice"),
                fill: Color32::from_rgba_premultiplied(104, 78, 16, 148),
                stroke: Color32::from_rgba_premultiplied(236, 197, 76, 166),
                text: Color32::from_rgb(255, 240, 184),
            }),
            config::AiInstallType::Full => None,
        }
    }
}

fn show_ai_install_notice(ui: &mut Ui, notice: AiInstallNotice) {
    Frame::new()
        .fill(notice.fill)
        .stroke(Stroke::new(1.0, notice.stroke))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(AI_INSTALL_NOTICE_WIDTH);
            // The web "Веб-версия" notice is longer (two paragraphs); give it room.
            #[cfg(not(target_arch = "wasm32"))]
            ui.set_max_height(AI_INSTALL_NOTICE_MAX_HEIGHT);
            #[cfg(target_arch = "wasm32")]
            ui.set_max_height(200.0);
            ui.add_sized(
                Vec2::new(AI_INSTALL_NOTICE_WIDTH - 32.0, 0.0),
                egui::Label::new(
                    RichText::new(notice.message)
                        .size(14.0)
                        .strong()
                        .color(notice.text),
                )
                .wrap(),
            );
        });
}

fn menu_top_space(viewport_height: f32, has_update_notice: bool) -> f32 {
    let base = (viewport_height * 0.17).max(24.0);
    if has_update_notice {
        base.max(UPDATE_NOTICE_TOP_MARGIN + UPDATE_NOTICE_OUTER_HEIGHT + UPDATE_NOTICE_GAP)
    } else {
        base
    }
}

fn show_update_notice(
    app: &LauncherApp,
    ui: &mut Ui,
    menu_top_space: f32,
) -> Option<PageNavAction> {
    let notification = app.update_notification.as_ref()?;
    let viewport = ui.max_rect();
    let pos = egui::pos2(
        viewport.center().x - UPDATE_NOTICE_OUTER_WIDTH * 0.5,
        viewport.top() + menu_top_space - UPDATE_NOTICE_OUTER_HEIGHT - UPDATE_NOTICE_GAP,
    );
    let mut action = None;

    Area::new("launcher_update_notice".into())
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(Color32::from_rgba_premultiplied(18, 20, 16, 166))
                .stroke(Stroke::new(
                    1.0,
                    Color32::from_rgba_premultiplied(225, 212, 122, 148),
                ))
                .corner_radius(egui::CornerRadius::same(12))
                .inner_margin(egui::Margin::symmetric(18, 14))
                .show(ui, |ui| {
                    ui.set_width(UPDATE_NOTICE_WIDTH);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(t!("launcher.main.update_available_heading"))
                                .size(24.0)
                                .strong()
                                .color(Color32::from_rgb(120, 230, 120)),
                        );
                        ui.add_space(4.0);
                        ui.label(theme::footer(&format!(
                            "{} -> {}",
                            notification.local_version, notification.remote_version
                        )));
                        ui.add_space(10.0);
                        let button = egui::Button::new(
                            RichText::new(t!("launcher.main.update_button"))
                                .size(17.0)
                                .strong()
                                .color(Color32::from_rgb(255, 248, 198)),
                        )
                        .min_size(egui::vec2(154.0, 38.0))
                        .fill(Color32::from_rgba_premultiplied(210, 180, 58, 112))
                        .stroke(Stroke::new(
                            1.0,
                            Color32::from_rgba_premultiplied(250, 230, 120, 190),
                        ));
                        if ui.add(button).clicked() {
                            action = Some(PageNavAction::StartUpdate);
                        }
                    });
                });
        });

    action
}

/// A main-menu button of the given width. Returns the full `Response` so the
/// caller can record its rect for the tutorial overlay before consuming clicks.
fn menu_button_response(ui: &mut Ui, label: &str, width: f32) -> egui::Response {
    theme::launcher_button(ui, label, egui::vec2(width, BUTTON_HEIGHT), true)
}

fn show_import_popup(
    app: &mut LauncherApp,
    ui: &mut Ui,
    import_button_rect: Option<egui::Rect>,
) -> Option<PageNavAction> {
    if !app.state.import_popup_open {
        return None;
    }

    let Some(button_rect) = import_button_rect else {
        app.state.import_popup_open = false;
        return None;
    };

    let popup_pos = egui::pos2(
        button_rect.center().x - IMPORT_POPUP_WIDTH * 0.5,
        button_rect.min.y - BUTTON_HEIGHT * 2.0 - IMPORT_POPUP_GAP - 18.0,
    );
    let mut action = None;
    let popup_response = Area::new("launcher_import_popup".into())
        .order(Order::Foreground)
        .fixed_pos(popup_pos)
        .show(ui.ctx(), |ui| {
            Frame::new()
                .fill(theme::CARD_FILL)
                .fill(egui::Color32::from_rgb(24, 24, 28))
                .stroke(Stroke::new(1.0, theme::CARD_STROKE))
                .corner_radius(egui::CornerRadius::same(12))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(IMPORT_POPUP_WIDTH);
                    ui.vertical(|ui| {
                        if theme::launcher_button(
                            ui,
                            t!("launcher.main.import_from_mschapter_option"),
                            egui::vec2(IMPORT_POPUP_WIDTH, BUTTON_HEIGHT),
                            true,
                        )
                        .clicked()
                        {
                            app.state.import_popup_open = false;
                            app.state.main_page_message = None;
                            action = Some(PageNavAction::Open(LauncherPage::ImportChapter));
                        }
                        if theme::launcher_button(
                            ui,
                            t!("launcher.main.import_from_psd_option"),
                            egui::vec2(IMPORT_POPUP_WIDTH, BUTTON_HEIGHT),
                            true,
                        )
                        .clicked()
                        {
                            app.state.import_popup_open = false;
                            app.state.main_page_message = None;
                            app.state.psd_import_window_open = true;
                        }
                    });
                });
        });

    // Mirror the old Qt popup: any click outside the trigger and popup closes it.
    let clicked_outside = ui.ctx().input(|input| {
        input.pointer.any_pressed()
            && !button_rect.contains(input.pointer.interact_pos().unwrap_or_default())
            && !popup_response
                .response
                .rect
                .contains(input.pointer.interact_pos().unwrap_or_default())
    });
    if clicked_outside {
        app.state.import_popup_open = false;
    }

    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_install_notice_matches_install_type() {
        assert!(ai_install_notice(config::AiInstallType::Full).is_none());

        let none_notice =
            ai_install_notice(config::AiInstallType::None).expect("None should show red notice");
        assert_eq!(none_notice.message, t!("launcher.main.offline_mode_notice"));

        let base_notice =
            ai_install_notice(config::AiInstallType::Base).expect("Base should show yellow notice");
        assert_eq!(base_notice.message, t!("launcher.main.lite_version_notice"));
    }
}
