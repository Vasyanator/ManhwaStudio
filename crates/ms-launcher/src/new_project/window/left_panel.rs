/*
File: crates/ms-launcher/src/new_project/window/left_panel.rs

Purpose:
Left control column of the New Project window: the full/simple mode tabs, the simple-mode
step wizard (import, stitch, denoise, save) with its navigation, the import section and the
quick downloader form.

Key items:
- `NewProjectWindowState::show_left_panel`: entry called from `show_contents`.
- `show_full_panel` / `show_simple_panel` and the `show_simple_*_step` bodies.
- `show_import_section`, `show_quick_downloader`.

Notes:
Draw-only: every button starts work through the `start_*` methods in `jobs.rs`, `capture.rs`
or `advanced_ui.rs`; nothing here blocks the GUI thread.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn show_left_panel(&mut self, ui: &mut Ui) {
        let any_loading = self.source_import.is_loading()
            || self.advanced_download.is_loading()
            || self.quick_download.is_loading()
            || self.test_chapter_check_rx.is_some()
            || self.stitch.is_loading()
            || self.save.is_loading()
            || self.waifu2x.is_loading()
            || self.reline.is_loading()
            || self.clipboard_paste_in_flight
            || self.screen_capture_in_flight;
        ui.set_width(LEFT_PANEL_WIDTH);
        ui.vertical(|ui| {
            ui.label(RichText::new(t!("launcher.new_project.overall_progress_label")).small());
            ui.label(RichText::new(&self.import_status).small().weak());
            let global_progress = self.current_progress(any_loading);
            ui.horizontal(|ui| {
                ui.add(
                    ProgressBar::new(global_progress.fraction)
                        .animate(any_loading)
                        .desired_width((LEFT_PANEL_WIDTH - 86.0).max(180.0))
                        .text(global_progress.label),
                );
                if button_sized(ui, t!("launcher.new_project.guide_button"), egui::vec2(64.0, 22.0), true).clicked() {
                    self.pending_open_wiki_guide = true;
                }
            });
            // A collapsed batch window draws nothing until restored: this button is its
            // restore button then (`open` also restores).
            let batch_label = if self.batch_window.is_collapsed() {
                t!("launcher.new_project.resume_batch_button")
            } else {
                t!("launcher.new_project.batch_processing_button")
            };
            if button_sized(ui, batch_label, egui::vec2(LEFT_PANEL_WIDTH - 4.0, 34.0), true).clicked() {
                self.batch_window.open(ui.ctx());
            }
            ui.add_space(10.0);
            self.show_left_panel_mode_tabs(ui);
            ui.add_space(10.0);
            match self.left_panel_mode {
                LeftPanelMode::Simple => self.show_simple_panel(ui),
                LeftPanelMode::Full => self.show_full_panel(ui),
            }
        });
    }

    pub(super) fn show_left_panel_mode_tabs(&mut self, ui: &mut Ui) {
        // `_row` (leading underscore) is only consumed by the feature-gated `mark`
        // below; the underscore keeps it warning-free when `tutorial` is off.
        let _row = ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Simple,
                t!("launcher.new_project.mode_simple"),
            );
            ui.selectable_value(
                &mut self.left_panel_mode,
                LeftPanelMode::Full,
                t!("launcher.new_project.mode_full"),
            );
        });
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_MODE_TABS, _row.response.rect);
    }

    pub(super) fn show_full_panel(&mut self, ui: &mut Ui) {
        ScrollArea::vertical()
            .id_salt("launcher_new_project_left_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                section_group(ui, t!("launcher.new_project.section_import"), |ui| self.show_import_section(ui));
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_quick_dl"), |ui| {
                    self.show_quick_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_advanced_dl"), |ui| {
                    self.show_advanced_downloader(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_stitch"), |ui| {
                    self.show_stitch_section(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.section_processing"), |ui| {
                    self.show_image_processing_section(ui)
                });
                ui.add_space(SECTION_SPACING);
                section_group(ui, t!("launcher.new_project.step_save"), |ui| self.show_save_section(ui));
            });
    }

    pub(super) fn show_simple_panel(&mut self, ui: &mut Ui) {
        self.show_simple_step_tabs(ui);
        ui.add_space(10.0);
        ScrollArea::vertical()
            .id_salt("launcher_new_project_simple_left_scroll")
            .auto_shrink([false, false])
            .max_height((ui.available_height() - 54.0).max(160.0))
            .show(ui, |ui| match self.simple_mode_step {
                SimpleModeStep::ImportDownload => self.show_simple_import_step(ui),
                SimpleModeStep::StitchCut => self.show_simple_stitch_step(ui),
                SimpleModeStep::Denoise => self.show_simple_denoise_step(ui),
                SimpleModeStep::Save => self.show_simple_save_step(ui),
            });
        ui.add_space(10.0);
        self.show_simple_step_navigation(ui);
    }

    pub(super) fn show_simple_step_tabs(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            for step in SimpleModeStep::ALL {
                let label = format!("{}. {}", step.number(), step.title());
                ui.selectable_value(&mut self.simple_mode_step, step, label);
            }
        });
    }

    pub(super) fn show_simple_import_step(&mut self, ui: &mut Ui) {
        let file_button_enabled =
            !self.source_import.is_loading() && !self.clipboard_paste_in_flight;
        let folder_button_enabled = !self.source_import.is_loading();
        let clipboard_button_enabled = !self.source_import.is_loading()
            && !self.clipboard_paste_in_flight
            && !self.screen_capture_in_flight;
        let quick_download_enabled = !self.source_import.is_loading()
            && !self.quick_download.is_loading()
            && self.test_chapter_check_rx.is_none();
        let advanced_button_enabled = !self.advanced_download.is_loading()
            && !self.advanced_link_collect_active
            && !self.advanced_intercept_active;

        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.simple_intro_text"),
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                ui.add_space(SECTION_SPACING);

                if self.simple_import_show_advanced {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.simple_adv_dl_open_notice"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button_sized(
                                ui,
                                t!("launcher.new_project.back_to_options_button"),
                                egui::vec2(176.0, 28.0),
                                true,
                            )
                            .clicked()
                            {
                                self.simple_import_show_advanced = false;
                            }
                        });
                    });
                    ui.add_space(10.0);
                    self.show_advanced_downloader(ui);
                    return;
                }

                sub_group(ui, t!("launcher.new_project.source_question"), |ui| {
                    ui.label(
                        RichText::new(t!("launcher.new_project.already_downloaded_hint")).strong(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_archive_image_button"),
                            egui::vec2(198.0, 34.0),
                            file_button_enabled,
                        )
                        .clicked()
                        {
                            self.open_file_dialog(ui.ctx());
                        }
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_folder_button"),
                            egui::vec2(146.0, 34.0),
                            folder_button_enabled,
                        )
                        .clicked()
                        {
                            self.open_folder_dialog(ui.ctx());
                        }
                    });
                    if button_sized(
                        ui,
                        t!("launcher.new_project.paste_from_clipboard_button"),
                        egui::vec2(198.0, 34.0),
                        clipboard_button_enabled,
                    )
                    .clicked()
                    {
                        self.start_clipboard_paste();
                    }
                    self.show_operation_progress(ui, "source");
                });

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.saved_webpage_hint"),
                    |ui| {
                        ui.horizontal(|ui| {
                            if button_sized(
                                ui,
                                t!("launcher.new_project.open_page_folder_button"),
                                egui::vec2(198.0, 34.0),
                                folder_button_enabled,
                            )
                            .clicked()
                            {
                                self.open_folder_dialog(ui.ctx());
                            }
                            if button_sized(
                                ui,
                                t!("launcher.new_project.open_page_html_button"),
                                egui::vec2(182.0, 34.0),
                                file_button_enabled,
                            )
                            .clicked()
                            {
                                self.open_file_dialog(ui.ctx());
                            }
                        });
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.free_chapter_sites_hint"),
                    |ui| {
                        egui::CollapsingHeader::new(t!("launcher.new_project.supported_sites_header")).id_salt("launcher.new_project.supported_sites_header")
                            .default_open(false)
                            .show(ui, |ui| {
                                for site in supported_quick_download_sites() {
                                    ui.label(RichText::new(site).small().weak());
                                }
                            });
                        ui.add_space(8.0);
                        ui.add(
                            TextEdit::singleline(&mut self.quick_link)
                                .desired_width(fill_width(ui))
                                .hint_text(t!("launcher.new_project.paste_chapter_link_placeholder")),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            t!("launcher.new_project.download_button"),
                            egui::vec2(140.0, 34.0),
                            quick_download_enabled,
                        )
                        .clicked()
                        {
                            self.start_quick_download();
                        }
                        self.show_operation_progress(ui, "quick_download");
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.test_chapter_intro"),
                    |ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.test_chapter_hint"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.add_space(8.0);
                        let test_dl = button_sized(
                            ui,
                            t!("launcher.new_project.download_test_chapter_button"),
                            egui::vec2(220.0, 34.0),
                            quick_download_enabled,
                        );
                        #[cfg(feature = "tutorial")]
                        self.tutorial
                            .mark(tutorial::TARGET_TEST_DOWNLOAD, test_dl.rect);
                        if test_dl.clicked() {
                            self.start_test_chapter_download();
                        }
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.auto_intercept_title"),
                    |ui| {
                        ui.label(
                            RichText::new(
                                t!("launcher.new_project.auto_intercept_hint"),
                            )
                            .small()
                            .weak(),
                        );
                        ui.add_space(8.0);
                        ui.add(
                            TextEdit::singleline(&mut self.advanced_page_url)
                                .desired_width(fill_width(ui))
                                .hint_text(t!("launcher.new_project.paste_chapter_link_placeholder")),
                        );
                        ui.add_space(8.0);

                        // Deep capture cannot run while another advanced command
                        // is in flight; the URL is only needed to open the page.
                        let capture_busy = self.advanced_download.is_loading();
                        let url_ready = !self.advanced_page_url.trim().is_empty();

                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_in_browser_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && !self.advanced_intercept_active && url_ready,
                        )
                        .clicked()
                        {
                            self.prepare_simple_deep_capture();
                            self.start_advanced_open();
                        }
                        ui.add_space(8.0);

                        if self.advanced_intercept_active {
                            let counts = self.advanced_intercept_counts;
                            ui.label(
                                RichText::new(tf!("launcher.new_project.intercept_counts_status", counts = counts.canvases, counts_2 = counts.images))
                                .color(egui::Color32::from_rgb(76, 175, 80))
                                .strong(),
                            );
                            ui.add_space(8.0);
                        }

                        if button_sized(
                            ui,
                            t!("launcher.new_project.start_intercept_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && !self.advanced_intercept_active,
                        )
                        .clicked()
                        {
                            self.prepare_simple_deep_capture();
                            self.start_advanced_deep_intercept();
                        }
                        if button_sized(
                            ui,
                            t!("launcher.new_project.finish_intercept_button"),
                            egui::vec2(220.0, 34.0),
                            !capture_busy && self.advanced_intercept_active,
                        )
                        .clicked()
                        {
                            self.finish_advanced_deep_intercept();
                        }
                        self.show_operation_progress(ui, "advanced_download");
                    },
                );

                ui.add_space(SECTION_SPACING);
                sub_group(
                    ui,
                    t!("launcher.new_project.all_methods_failed_hint"),
                    |ui| {
                        ui.label(
                            RichText::new(t!("launcher.new_project.html_dig_hint"))
                                .small()
                                .weak(),
                        );
                        ui.add_space(8.0);
                        if button_sized(
                            ui,
                            t!("launcher.new_project.open_advanced_dl_button"),
                            egui::vec2(260.0, 34.0),
                            advanced_button_enabled,
                        )
                        .clicked()
                        {
                            self.simple_import_show_advanced = true;
                        }
                    },
                );
            },
        );
    }

    pub(super) fn show_simple_stitch_step(&mut self, ui: &mut Ui) {
        let can_start = self.can_start_stitch();
        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    t!("launcher.new_project.vertical_or_paged_question"),
                );
                ui.add_space(8.0);
                if button_sized(ui, t!("launcher.new_project.skip_stitch_button"), egui::vec2(220.0, 34.0), true).clicked()
                {
                    self.simple_stitch_done = true;
                    self.simple_manual_cut_preview_active = false;
                    self.simple_mode_step = SimpleModeStep::Denoise;
                }

                ui.add_space(SECTION_SPACING * 1.6);
                ui.label(RichText::new(t!("launcher.new_project.if_webtoon_cut_hint")).strong());
                ui.add_space(8.0);
                if button_sized(
                    ui,
                    t!("launcher.new_project.stitch_cut_auto_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_start,
                )
                .clicked()
                {
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.start_stitch_split(StitchSplitMode::AutoCut);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.stitch_show_cuts_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_start,
                )
                .clicked()
                {
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.start_stitch_split(StitchSplitMode::ManualCutPreview);
                }
                self.show_operation_progress(ui, "stitch");

                if self.simple_manual_cut_preview_active {
                    ui.add_space(SECTION_SPACING);
                    ui.label(
                        RichText::new(
                            t!("launcher.new_project.auto_cuts_review_hint"),
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                }

                if self.simple_stitch_done {
                    ui.add_space(SECTION_SPACING);
                    ui.label(
                        RichText::new(
                            t!("launcher.new_project.extra_cuts_hint"),
                        )
                        .color(egui::Color32::WHITE)
                        .strong(),
                    );
                    ui.add_space(10.0);
                    if button_sized(ui, t!("launcher.common.next_button"), egui::vec2(124.0, 34.0), true).clicked() {
                        self.simple_mode_step = SimpleModeStep::Denoise;
                    }
                }
            },
        );
    }

    pub(super) fn show_simple_denoise_step(&mut self, ui: &mut Ui) {
        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.denoise_optional_hint"),
                    )
                    .color(egui::Color32::WHITE)
                    .strong(),
                );
                self.show_operation_progress(ui, self.current_image_processing_operation());

                ui.add_space(SECTION_SPACING * 1.6);
                if button_sized(
                    ui,
                    &tf!("launcher.new_project.process_with_button", arg = self.image_processor.title()),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    self.can_start_image_processing(),
                )
                .clicked()
                {
                    self.start_image_processing();
                    self.simple_mode_step = SimpleModeStep::Save;
                }
                if button_sized(ui, t!("launcher.new_project.skip_button"), egui::vec2(140.0, 34.0), true).clicked()
                {
                    self.simple_mode_step = SimpleModeStep::Save;
                }
            },
        );
    }

    pub(super) fn show_simple_save_step(&mut self, ui: &mut Ui) {
        let titles = self.project_catalog_snapshot.titles.clone();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.save.is_loading();
        let can_save = self.can_start_save();

        section_group(
            ui,
            &tf!("launcher.new_project.simple_step_header", arg = self.simple_mode_step.number(), arg_2 = self.simple_mode_step.title()),
            |ui| {
                let mut save_to_project = self.save_mode != SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_project, t!("launcher.new_project.save_to_projects_button"))
                    .changed()
                    && save_to_project
                {
                    self.save_mode = SaveMode::ProjectBase;
                }

                let mut save_to_folder = self.save_mode == SaveMode::Independent;
                if ui
                    .checkbox(&mut save_to_folder, t!("launcher.new_project.save_to_other_folder_button"))
                    .changed()
                    && save_to_folder
                {
                    self.save_mode = SaveMode::Independent;
                }
                ui.label(
                    RichText::new(t!("launcher.new_project.just_downloading_hint"))
                        .small()
                        .weak(),
                );

                ui.add_space(SECTION_SPACING);
                if self.project_catalog.is_loading() {
                    ui.label(
                        RichText::new(t!("launcher.new_project.refreshing_titles_status"))
                            .small()
                            .weak(),
                    );
                } else if let Some(error) = &self.project_catalog_error {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
                }

                if self.save_mode == SaveMode::Independent {
                    if button_sized(
                        ui,
                        t!("launcher.new_project.choose_save_folder_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_folder(ui.ctx());
                    }
                    self.show_operation_progress(ui, "save");
                    return;
                }

                ui.label(t!("launcher.new_project.title_input_hint"));
                let save_title_response =
                    self.save_title_combo
                        .draw(ui, &mut self.save_title_input, titles.as_slice());
                if save_title_response.changed {
                    self.sync_save_title_from_input();
                }
                if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog).clicked()
                {
                    self.refresh_project_catalog();
                }

                ui.add_space(8.0);
                ui.label(t!("launcher.new_project.chapter_input_hint"));
                ui.add(TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)));

                ui.add_space(SECTION_SPACING);
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_and_open_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_project(true, false);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_and_download_more_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_project(false, true);
                }

                ui.add_space(SECTION_SPACING);
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.projects_location_hint"),
                    )
                    .small()
                    .weak(),
                );
                self.show_operation_progress(ui, "save");
            },
        );
    }

    pub(super) fn show_simple_step_navigation(&mut self, ui: &mut Ui) {
        let previous_step = self.simple_mode_step.previous();
        let next_step = self.simple_mode_step.next();
        let can_go_next = next_step.is_some()
            && (self.simple_mode_step != SimpleModeStep::StitchCut || self.simple_stitch_done);
        let next_label = if self.simple_mode_step == SimpleModeStep::StitchCut {
            t!("launcher.common.next_button")
        } else {
            t!("launcher.common.forward_button")
        };
        ui.horizontal(|ui| {
            if button_sized(
                ui,
                t!("launcher.common.back_button"),
                egui::vec2(124.0, 34.0),
                previous_step.is_some(),
            )
            .clicked()
                && let Some(step) = previous_step
            {
                self.simple_mode_step = step;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if button_sized(ui, next_label, egui::vec2(124.0, 34.0), can_go_next).clicked()
                    && let Some(step) = next_step
                {
                    self.simple_mode_step = step;
                }
            });
        });
    }

    pub(super) fn advance_simple_import_step_after_success(&mut self) {
        if self.left_panel_mode == LeftPanelMode::Simple
            && self.simple_mode_step == SimpleModeStep::ImportDownload
        {
            self.simple_import_show_advanced = false;
            self.simple_mode_step = SimpleModeStep::StitchCut;
        }
    }

    pub(super) fn show_import_section(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let open_folder = button_sized(
                ui,
                t!("launcher.new_project.open_folder_menu"),
                ACTION_BUTTON_SIZE,
                !self.source_import.is_loading(),
            );
            #[cfg(feature = "tutorial")]
            self.tutorial.mark(tutorial::TARGET_IMPORT, open_folder.rect);
            if open_folder.clicked() {
                self.open_folder_dialog(ui.ctx());
            }
            if button_sized(
                ui,
                t!("launcher.new_project.open_file_menu"),
                ACTION_BUTTON_SIZE,
                !self.source_import.is_loading() && !self.clipboard_paste_in_flight,
            )
            .clicked()
            {
                self.open_file_dialog(ui.ctx());
            }
        });
        if button_sized(
            ui,
            t!("launcher.new_project.paste_from_clipboard_button"),
            ACTION_BUTTON_SIZE,
            !self.source_import.is_loading()
                && !self.clipboard_paste_in_flight
                && !self.screen_capture_in_flight,
        )
        .clicked()
        {
            self.start_clipboard_paste();
        }
        let capture_button_label = if self.is_screen_capture_mode_enabled() {
            t!("launcher.new_project.exit_capture_mode_button")
        } else {
            t!("launcher.new_project.capture_mode_button")
        };
        if button_sized(
            ui,
            capture_button_label,
            ACTION_BUTTON_SIZE,
            SCREEN_CAPTURE_UI_ENABLED
                && !self.source_import.is_loading()
                && !self.clipboard_paste_in_flight
                && !self.screen_capture_in_flight,
        )
        .clicked()
        {
            if self.is_screen_capture_mode_enabled() {
                self.stop_screen_capture_mode();
            } else {
                self.start_screen_capture_mode(ui.ctx());
            }
        }
        if !SCREEN_CAPTURE_UI_ENABLED {
            ui.label(
                RichText::new(t!("launcher.new_project.capture_mode_disabled_notice"))
                    .small()
                    .weak(),
            );
        }
        field_label(ui, t!("launcher.new_project.import_mode_label"));
        let mut import_mode_index = self.import_mode.as_index();
        combo_index(
            ui,
            "launcher_new_project_import_mode",
            &[
                t!("launcher.new_project.import_mode_replace"),
                t!("launcher.new_project.import_mode_prepend"),
                t!("launcher.new_project.import_mode_append"),
                t!("launcher.new_project.import_mode_before_current"),
                t!("launcher.new_project.import_mode_after_current"),
            ],
            &mut import_mode_index,
        );
        self.import_mode = ImportMode::from_index(import_mode_index);
        ui.checkbox(
            &mut self.filter_same_width,
            t!("launcher.new_project.filter_same_width_check"),
        );
        field_label(ui, t!("launcher.new_project.extra_filenames_hint"));
        ui.add(
            TextEdit::singleline(&mut self.import_extra_names)
                .desired_width(fill_width(ui))
                .hint_text("resource, scan*.*, page????"),
        );
        self.show_operation_progress(ui, "source");
    }

    pub(super) fn show_quick_downloader(&mut self, ui: &mut Ui) {
        field_label(ui, t!("launcher.new_project.chapter_link_label"));
        ui.add(
            TextEdit::singleline(&mut self.quick_link)
                .desired_width(fill_width(ui))
                .hint_text(t!("launcher.new_project.chapter_link_placeholder")),
        );
        ui.add_space(6.0);
        let can_start_download =
            !self.source_import.is_loading() && !self.quick_download.is_loading();
        let response = button_sized(
            ui,
            t!("launcher.new_project.load_chapters_from_link_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_start_download,
        );
        let response = response.on_hover_text(supported_sites_tooltip());
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_QUICK, response.rect);
        if response.clicked() {
            self.start_quick_download();
        }
        self.show_operation_progress(ui, "quick_download");
    }
}

