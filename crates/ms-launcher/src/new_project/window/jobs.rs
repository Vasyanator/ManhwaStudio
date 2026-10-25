/*
File: crates/ms-launcher/src/new_project/window/jobs.rs

Purpose:
Background-job wiring of the New Project window: the `start_*` methods that hand work to the
controllers and the `poll_*` methods that apply their events on the GUI thread, plus the
test-chapter site check and the progress stage/operation titles.

Key items:
- `open_folder_dialog` / `open_file_dialog`, `start_quick_download`, `start_test_chapter_download`,
  `start_waifu2x`, `start_reline`, `start_image_processing`.
- `poll_*` for every controller (called from `NewProjectWindowState::poll`).
- `handle_window_closed`, `cancel_quick_download_slot`.
- `check_test_chapter_site_availability` (wasm and native twins), `progress_stage_title`.

Notes:
The GUI thread only polls channels here; the work itself runs in the controllers.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    /// Opens the source folder picker; `poll_folder_load` shows the import progress once a
    /// folder is chosen.
    pub(super) fn open_folder_dialog(&mut self, ctx: &egui::Context) {
        self.source_import
            .begin_pick(OpenSourceKind::Folder, self.source_import_options(), Some(repaint_waker(ctx)));
    }

    /// Opens the source file picker (archive, HTML or image); see `open_folder_dialog`.
    pub(super) fn open_file_dialog(&mut self, ctx: &egui::Context) {
        self.source_import
            .begin_pick(OpenSourceKind::File, self.source_import_options(), Some(repaint_waker(ctx)));
    }

    pub(super) fn start_quick_download(&mut self) {
        let url = self.quick_link.trim();
        if url.is_empty() {
            self.last_error = Some(t!("launcher.new_project.paste_link_before_download_error").to_string());
            self.import_status = t!("launcher.new_project.quick_dl_waiting_link").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "quick_download",
            stage: "download".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = t!("launcher.new_project.quick_dl_preparing_status").to_string();
        self.quick_download.begin_download(url.to_string());
    }

    pub(super) fn start_test_chapter_download(&mut self) {
        if self.test_chapter_check_rx.is_some() || self.quick_download.is_loading() {
            return;
        }
        let (tx, rx) = mpsc::channel::<TestChapterAvailabilityResult>();
        self.test_chapter_check_rx = Some(rx);
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "quick_download",
            stage: "connect".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = t!("launcher.new_project.checking_naver_status").to_string();
        let chapter_number = random_test_chapter_number();
        let chapter_url =
            format!("https://comic.naver.com/webtoon/detail?titleId=842647&no={chapter_number}");
        thread::spawn(move || {
            let result = check_test_chapter_site_availability(chapter_url);
            // The receiver is dropped only when the window closed (`cancel_quick_download_slot`),
            // so an undeliverable result is a cancelled check, not a failure.
            if tx.send(result).is_err() {
                ms_log::runtime_log::log_info("[new-project] test chapter availability check finished after the window was closed; result dropped");
            }
        });
    }

    pub(super) fn start_waifu2x(&mut self) {
        let Some(options) = self.parse_waifu2x_options() else {
            return;
        };
        let images = self.current_waifu2x_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_first_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "waifu2x",
            stage: "prepare".to_string(),
            current: 0,
            total: images.len(),
        });
        self.import_status = t!("launcher.new_project.preparing_waifu2x_status").to_string();
        self.waifu2x.begin(images, options);
    }

    pub(super) fn start_reline(&mut self) {
        let options = match self.reline_ui_mode {
            RelineUiMode::Simple => self.build_reline_simple_options(),
            RelineUiMode::Full => self.parse_reline_options(),
        };
        let Some(options) = options else {
            return;
        };
        let images = self.current_reline_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_first_error").to_string());
            self.import_status = t!("launcher.new_project.reline_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "reline",
            stage: "prepare".to_string(),
            current: 0,
            total: images.len(),
        });
        self.import_status = t!("launcher.new_project.sending_to_reline_status").to_string();
        self.reline.begin(images, options);
    }

    pub(super) fn start_image_processing(&mut self) {
        match self.image_processor {
            ImageProcessor::Waifu2x => self.start_waifu2x(),
            ImageProcessor::Reline => self.start_reline(),
        }
    }

    pub(super) fn poll_folder_load(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.source_import.poll(ctx) {
            match event {
                SourceLoadEvent::LoadStarted { kind } => {
                    self.manual_cut_guides.clear();
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "source",
                        stage: "scan".to_string(),
                        current: 0,
                        total: 1,
                    });
                    self.import_status = match kind {
                        OpenSourceKind::Folder => t!("launcher.new_project.scanning_folder_status"),
                        OpenSourceKind::File => t!("launcher.new_project.opening_file_status"),
                    }
                    .to_string();
                }
                SourceLoadEvent::PickFailed {
                    user_message,
                    log_message,
                } => {
                    // The ribbon is untouched: only the picker failed, nothing was loaded.
                    ms_log::runtime_log::log_warn(format!("[new-project] {log_message}"));
                    self.last_error = Some(user_message);
                }
                SourceLoadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "source",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                SourceLoadEvent::Loaded(result) => {
                    let page_count = result.pages.len();
                    self.apply_source_import_result(result.source_path.clone(), result.pages);
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] imported {} ribbon images from '{}' (skipped={}, filtered_out={})",
                        page_count,
                        result.source_path.display(),
                        result.skipped_files,
                        result.filtered_out,
                    ));
                    self.import_status = if let Some((median, min_width, max_width)) =
                        result.filter_bounds
                    {
                        tf!("launcher.new_project.import_summary_with_median", result = result.imported_images, result_2 = result.source_path.display(), result_3 = result.skipped_files, result_4 = result.filtered_out, median = median, min_width = min_width, max_width = max_width)
                    } else {
                        tf!("launcher.new_project.import_summary", result = result.imported_images, result_2 = result.source_path.display(), result_3 = result.skipped_files, result_4 = result.filtered_out)
                    };
                    self.last_error = None;
                }
                SourceLoadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] source import failed: {}",
                        log_message
                    ));
                    self.ribbon.clear();
                    self.selected_ribbon_page = None;
                    self.crop_editor = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.load_source_error").to_string();
                    self.last_error = Some(user_message);
                }
                SourceLoadEvent::WorkerDisconnected => {
                    ms_log::runtime_log::log_error(
                        "[new-project] source import worker disconnected unexpectedly",
                    );
                    self.ribbon.clear();
                    self.selected_ribbon_page = None;
                    self.crop_editor = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.load_source_error").to_string();
                    self.last_error = Some(
                        t!("launcher.new_project.load_source_bg_error")
                            .to_string(),
                    );
                }
            }
        }
    }

    pub(super) fn poll_project_catalog(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.project_catalog.poll(ctx) {
            match event {
                ProjectCatalogEvent::Loaded(snapshot) => {
                    self.project_catalog_error = None;
                    self.project_catalog_snapshot = snapshot;
                    self.clamp_project_catalog_indexes();
                    if self
                        .project_catalog_snapshot
                        .titles
                        .get(self.save_title)
                        .is_some_and(|_| {
                            let current = self.save_title_input.trim();
                            current.is_empty() || current == "Title A"
                        })
                    {
                        self.save_title_input =
                            self.project_catalog_snapshot.titles[self.save_title].clone();
                    }
                    self.sync_save_title_from_input();
                }
                ProjectCatalogEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.project_catalog_error = Some(user_message);
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] project catalog refresh failed: {log_message}"
                    ));
                }
                ProjectCatalogEvent::WorkerDisconnected => {
                    self.project_catalog_error = Some(
                        t!("launcher.new_project.read_projects_bg_error").to_string(),
                    );
                    ms_log::runtime_log::log_error(
                        "[new-project] project catalog worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    pub(super) fn poll_test_chapter_check(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.test_chapter_check_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(TestChapterAvailabilityResult {
                available: false,
                chapter_url: String::new(),
                log_message: Some("test chapter availability worker disconnected".to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.test_chapter_check_rx = None;
        self.active_progress = None;

        if result.available {
            self.quick_link = result.chapter_url;
            self.start_quick_download();
            ctx.request_repaint();
            return;
        }

        if let Some(log_message) = result.log_message {
            ms_log::runtime_log::log_error(format!(
                "[new-project] comic.naver.com availability check failed: {log_message}"
            ));
        }
        self.import_status = t!("launcher.new_project.test_chapter_download_failed").to_string();
        self.last_error = Some(
            t!("launcher.new_project.naver_unavailable_vpn_hint")
                .to_string(),
        );
    }

    pub(super) fn poll_quick_download(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.quick_download.poll(ctx) {
            match event {
                QuickDownloadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "quick_download",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                QuickDownloadEvent::Loaded(result) => {
                    let page_count = result.pages.len();
                    self.ribbon
                        .replace_source(PathBuf::from(&result.source_url), result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.active_progress = None;
                    self.advance_simple_import_step_after_success();
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] quick-downloaded {} ribbon images from '{}'",
                        page_count, result.source_url,
                    ));
                    self.import_status = tf!("launcher.new_project.quick_dl_done_status", result = result.downloaded_images, result_2 = result.source_url);
                    self.last_error = None;
                }
                QuickDownloadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] quick downloader failed: {log_message}",
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.quick_dl_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                QuickDownloadEvent::WorkerDisconnected => {
                    ms_log::runtime_log::log_error(
                        "[new-project] quick download worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.quick_dl_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.quick_dl_bg_error").to_string());
                }
            }
        }
    }

    pub(super) fn poll_advanced_download(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.advanced_download.poll(ctx) {
            match event {
                AdvancedDownloadEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "advanced_download",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                AdvancedDownloadEvent::BrowserOpened { current_url } => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status =
                        tf!("launcher.new_project.browser_opened_status", current_url = current_url);
                }
                AdvancedDownloadEvent::LinkCollectStarted { current_url } => {
                    self.advanced_link_collect_active = true;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_link_collect_last_poll_at =
                        Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.collect_started_status", current_url = current_url);
                }
                AdvancedDownloadEvent::LinkCollectCountUpdated { found_links } => {
                    self.advanced_link_collect_found_links = found_links;
                    self.advanced_link_collect_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::InterceptStarted { current_url } => {
                    self.advanced_intercept_active = true;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    self.advanced_intercept_last_poll_at = Instant::now() - Duration::from_secs(1);
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = if self.advanced_mode == AdvancedDownloadMode::DeepCapture
                    {
                        tf!("launcher.new_project.deep_intercept_started_status", current_url = current_url)
                    } else {
                        tf!("launcher.new_project.canvas_intercept_started_status", current_url = current_url)
                    };
                }
                AdvancedDownloadEvent::InterceptCountUpdated { counts } => {
                    self.advanced_intercept_counts = counts;
                    self.advanced_intercept_last_poll_at = Instant::now();
                }
                AdvancedDownloadEvent::Loaded(result) => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    let page_count = result.pages.len();
                    self.ribbon
                        .replace_source(PathBuf::from(&result.source_url), result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.simple_stitch_done = false;
                    self.simple_manual_cut_preview_active = false;
                    self.advance_simple_import_step_after_success();
                    self.import_status =
                        if self.advanced_mode == AdvancedDownloadMode::CanvasDownload {
                            tf!("launcher.new_project.canvas_mode_done_status", result = result.downloaded_images, result_2 = result.source_url)
                        } else {
                            tf!("launcher.new_project.advanced_dl_done_status", result = result.downloaded_images, result_2 = result.source_url)
                        };
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] advanced downloader loaded {page_count} ribbon images from '{}'",
                        result.source_url,
                    ));
                }
                AdvancedDownloadEvent::AutoCandidatesReady(candidates) => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    let item_count = candidates.items.len();
                    let group_count = candidates.groups.len();
                    self.advanced_auto_review = Some(AdvancedAutoReviewState::new(candidates));
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.autofind_prepared_status", item_count = item_count, group_count = group_count);
                }
                AdvancedDownloadEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] advanced downloader failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status =
                        t!("launcher.new_project.advanced_dl_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                AdvancedDownloadEvent::WorkerDisconnected => {
                    self.advanced_link_collect_active = false;
                    self.advanced_link_collect_found_links = 0;
                    self.advanced_intercept_active = false;
                    self.advanced_intercept_counts = InterceptCounts::default();
                    ms_log::runtime_log::log_error(
                        "[new-project] advanced downloader worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status =
                        t!("launcher.new_project.advanced_dl_failed_error").to_string();
                    self.last_error = Some(
                        t!("launcher.new_project.advanced_dl_bg_error").to_string(),
                    );
                }
            }
        }

        self.poll_advanced_link_collect_status(ctx);
        self.poll_advanced_intercept_status(ctx);
    }

    pub(super) fn poll_advanced_link_collect_status(&mut self, ctx: &egui::Context) {
        if !self.advanced_link_collect_active || self.advanced_download.has_pending_command() {
            return;
        }
        if self.advanced_link_collect_last_poll_at.elapsed() < Duration::from_millis(350) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            return;
        };
        self.advanced_link_collect_last_poll_at = Instant::now();
        self.advanced_download
            .begin_query_link_collect_count(browser);
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    pub(super) fn poll_advanced_intercept_status(&mut self, ctx: &egui::Context) {
        if !self.advanced_intercept_active || self.advanced_download.has_pending_command() {
            return;
        }
        if self.advanced_intercept_last_poll_at.elapsed() < Duration::from_millis(350) {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            return;
        };
        self.advanced_intercept_last_poll_at = Instant::now();
        if self.advanced_mode == AdvancedDownloadMode::DeepCapture {
            self.advanced_download
                .begin_query_deep_intercept_count(browser);
        } else {
            self.advanced_download
                .begin_query_canvas_intercept_count(browser);
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    pub(super) fn poll_stitch(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.stitch.poll(ctx) {
            match event {
                StitchEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "stitch",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                StitchEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides = result.cut_guides;
                    self.clamp_manual_cut_guides_to_current_pages();
                    self.active_progress = None;
                    self.last_error = None;
                    self.simple_manual_cut_preview_active =
                        matches!(&result.kind, StitchSuccessKind::ManualPreview);
                    self.simple_stitch_done = matches!(
                        &result.kind,
                        StitchSuccessKind::AutoCut
                            | StitchSuccessKind::ReferenceCut
                            | StitchSuccessKind::ManualApply
                    );
                    self.import_status = match result.kind {
                        StitchSuccessKind::AutoCut => {
                            tf!("launcher.new_project.stitch_done_status", page_count = page_count)
                        }
                        StitchSuccessKind::StitchOnly => {
                            tf!("launcher.new_project.stitch_ribbon_done_status", page_count = page_count)
                        }
                        StitchSuccessKind::HeterogeneousBottoms => tf!("launcher.new_project.stitch_uneven_done_status", page_count = page_count),
                        StitchSuccessKind::ReferenceCut => tf!("launcher.new_project.cut_by_example_done_status", page_count = page_count),
                        StitchSuccessKind::ManualPreview => tf!("launcher.new_project.stitch_manual_lines_done_status", arg = self.manual_cut_guides.len()),
                        StitchSuccessKind::ManualApply => {
                            tf!("launcher.new_project.manual_cut_done_status", page_count = page_count)
                        }
                    };
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] stitch worker completed: kind={}, pages={}, guides={}",
                        stitch_kind_name(&result.kind),
                        page_count,
                        self.manual_cut_guides.len(),
                    ));
                }
                StitchEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] stitch worker failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.stitch_cut_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                StitchEvent::WorkerDisconnected => {
                    ms_log::runtime_log::log_error(
                        "[new-project] stitch worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.stitch_cut_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.stitch_cut_bg_error").to_string());
                }
            }
        }
    }

    pub(super) fn poll_save(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.save.poll(ctx) {
            match event {
                ProjectSaveEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "save",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                ProjectSaveEvent::Completed(result) => {
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.save_done_status", result = result.saved_images, result_2 = result.target_dir.display());
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] save completed: files={}, dir='{}'",
                        result.saved_images,
                        result.target_dir.display()
                    ));
                    if self.open_after_save_requested {
                        if let Some(selection) = result.open_selection {
                            self.pending_open_selection = Some(selection);
                        } else {
                            self.pending_open_selection = None;
                        }
                    } else {
                        self.pending_open_selection = None;
                    }
                    if self.return_to_import_after_save_requested {
                        self.simple_mode_step = SimpleModeStep::ImportDownload;
                        self.simple_import_show_advanced = false;
                        self.simple_stitch_done = false;
                        self.simple_manual_cut_preview_active = false;
                    }
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.refresh_project_catalog();
                }
                ProjectSaveEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.pending_open_selection = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.save_failed_error").to_string();
                    self.last_error = Some(user_message);
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] save failed: {log_message}"
                    ));
                }
                ProjectSaveEvent::WorkerDisconnected => {
                    self.open_after_save_requested = false;
                    self.return_to_import_after_save_requested = false;
                    self.pending_open_selection = None;
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.save_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.save_bg_error").to_string());
                    ms_log::runtime_log::log_error(
                        "[new-project] save worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    pub(super) fn poll_waifu2x(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.waifu2x.poll(ctx) {
            match event {
                Waifu2xEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "waifu2x",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                Waifu2xEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.waifu2x_done_status", result = result.processed_images, result_2 = result.backend_path.display());
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] waifu2x completed: pages={}, backend='{}'",
                        page_count,
                        result.backend_path.display()
                    ));
                }
                Waifu2xEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] waifu2x failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.waifu2x.failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                Waifu2xEvent::WorkerDisconnected => {
                    ms_log::runtime_log::log_error(
                        "[new-project] waifu2x worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.waifu2x.failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.waifu2x_bg_error").to_string());
                }
            }
        }
    }

    pub(super) fn poll_reline(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.reline.poll(ctx) {
            match event {
                RelineEvent::Progress {
                    stage,
                    current,
                    total,
                } => {
                    self.last_error = None;
                    self.active_progress = Some(OperationProgress {
                        operation: "reline",
                        stage: stage.clone(),
                        current,
                        total,
                    });
                    self.import_status = progress_status_label(&stage, current, total);
                }
                RelineEvent::Completed(result) => {
                    let page_count = result.pages.len();
                    self.ribbon.replace_current(result.pages);
                    self.selected_ribbon_page = self.default_selected_page();
                    self.crop_editor = None;
                    self.manual_cut_guides.clear();
                    self.active_progress = None;
                    self.last_error = None;
                    self.import_status = tf!("launcher.new_project.reline_done_status", result = result.processed_images, result_2 = result.backend_endpoint);
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] Reline completed: pages={}, endpoint='{}'",
                        page_count, result.backend_endpoint
                    ));
                }
                RelineEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] Reline failed: {log_message}"
                    ));
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.reline_failed_error").to_string();
                    self.last_error = Some(user_message);
                }
                RelineEvent::WorkerDisconnected => {
                    ms_log::runtime_log::log_error(
                        "[new-project] Reline worker disconnected unexpectedly",
                    );
                    self.active_progress = None;
                    self.import_status = t!("launcher.new_project.reline_failed_error").to_string();
                    self.last_error =
                        Some(t!("launcher.new_project.reline_bg_error").to_string());
                }
            }
        }
    }

    pub(super) fn poll_reline_model_catalog(&mut self, ctx: &egui::Context) {
        if let Some(event) = self.reline_model_catalog.poll(ctx) {
            match event {
                RelineModelCatalogEvent::Loaded(models) => {
                    self.reline_model_catalog_error = None;
                    self.reline_model_catalog_entries = models;
                    if self.reline_model_name.trim().is_empty()
                        && let Some(first_model) = self.reline_model_catalog_entries.first()
                    {
                        self.reline_model_name = first_model.name.clone();
                    }
                    ms_log::runtime_log::log_info(format!(
                        "[new-project] loaded Reline model catalog: models={}",
                        self.reline_model_catalog_entries.len()
                    ));
                }
                RelineModelCatalogEvent::Failed {
                    user_message,
                    log_message,
                } => {
                    self.reline_model_catalog_error = Some(user_message);
                    ms_log::runtime_log::log_error(format!(
                        "[new-project] Reline model catalog failed: {log_message}"
                    ));
                }
                RelineModelCatalogEvent::WorkerDisconnected => {
                    self.reline_model_catalog_error = Some(
                        t!("launcher.new_project.reline_models_bg_error").to_string(),
                    );
                    ms_log::runtime_log::log_error(
                        "[new-project] Reline model catalog worker disconnected unexpectedly",
                    );
                }
            }
        }
    }

    /// Resets the per-opening state when the window closes: pending questions and pickers,
    /// the quick download and its test-chapter check (both cancelled), and the waifu2x / Reline
    /// controllers.
    pub(super) fn handle_window_closed(&mut self) {
        // A question or a picker answer belongs to the session that asked; never carry it into
        // the next opening of the window.
        self.overwrite_confirm = None;
        self.cut_like_folder_dialog = None;
        self.save_folder_dialog = None;
        // A quick download — and the test-chapter check that would start one — must not outlive
        // the window that asked for it: stop the worker / drop the check's receiver and release
        // their shared progress slot, so a reopened window shows neither a stale bar nor a late
        // result (a check finishing later would otherwise start a download in a closed window).
        let download_cancelled = self.quick_download.cancel();
        if cancel_quick_download_slot(&mut self.test_chapter_check_rx, download_cancelled, &mut self.active_progress) {
            self.import_status = t!("launcher.new_project.quick_dl_cancelled_status").to_string();
        }
        self.waifu2x.shutdown();
        self.waifu2x = Waifu2xController::new();
        self.reline = RelineController::new();
        self.reline_model_catalog = RelineModelCatalogController::new();
    }
}

/// Cancels whatever holds the shared "quick_download" progress slot when the window closes: the
/// test-chapter check is cancelled by dropping its receiver (`check_rx` becomes `None`; the
/// probe's single bounded request runs out on its worker and its result is discarded), and
/// `download_cancelled` says whether the quick-download worker was just stopped. When either
/// was cancelled, a "quick_download" progress is cleared and `true` is returned (the caller
/// reports the cancellation); otherwise nothing changes.
pub(super) fn cancel_quick_download_slot(
    check_rx: &mut Option<Receiver<TestChapterAvailabilityResult>>,
    download_cancelled: bool,
    progress: &mut Option<OperationProgress>,
) -> bool {
    let check_cancelled = check_rx.take().is_some();
    if check_cancelled {
        ms_log::runtime_log::log_info("[new-project] test chapter availability check cancelled: the window was closed");
    }
    if !check_cancelled && !download_cancelled {
        return false;
    }
    if progress.as_ref().is_some_and(|progress| progress.operation == "quick_download") {
        *progress = None;
    }
    true
}

pub(super) fn random_test_chapter_number() -> usize {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    usize::try_from(nanos % 100).unwrap_or(0).saturating_add(1)
}

/// Web stub: the availability probe uses a native HTTP client (`ureq`) that is not
/// compiled for wasm. Returns an unavailable result with a diagnostic log message
/// rather than a fake "reachable" answer.
#[cfg(target_arch = "wasm32")]
pub(super) fn check_test_chapter_site_availability(chapter_url: String) -> TestChapterAvailabilityResult {
    TestChapterAvailabilityResult {
        available: false,
        chapter_url,
        log_message: Some(
            t!("launcher.new_project.site_check_web_unsupported").to_string(),
        ),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn check_test_chapter_site_availability(chapter_url: String) -> TestChapterAvailabilityResult {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .timeout_read(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .timeout_write(TEST_CHAPTER_SITE_CHECK_TIMEOUT)
        .build();
    let request = agent.get("https://comic.naver.com/").set(
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36",
    );
    match request.call() {
        Ok(_) => TestChapterAvailabilityResult {
            available: true,
            chapter_url,
            log_message: None,
        },
        Err(ureq::Error::Status(status, response)) => TestChapterAvailabilityResult {
            available: false,
            chapter_url,
            log_message: Some(format!(
                "comic.naver.com returned status {status}; body={}",
                response.into_string().unwrap_or_default()
            )),
        },
        Err(ureq::Error::Transport(transport)) => TestChapterAvailabilityResult {
            available: false,
            chapter_url,
            log_message: Some(format!("comic.naver.com transport error: {transport}")),
        },
    }
}

pub(super) fn progress_operation_title(operation: &str) -> &'static str {
    match operation {
        "source" => t!("launcher.new_project.progress_download_images"),
        "save" => t!("launcher.new_project.progress_save"),
        "advanced_download" => t!("launcher.new_project.progress_advanced_dl"),
        "quick_download" => t!("launcher.new_project.progress_quick_dl"),
        "stitch" => t!("launcher.new_project.progress_cut"),
        "waifu2x" => "waifu2x",
        "reline" => "Reline",
        _ => t!("launcher.new_project.progress_processing"),
    }
}

pub(super) fn supported_quick_download_sites() -> Vec<&'static str> {
    supported_sites_tooltip()
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

pub(super) fn progress_stage_title(stage: &str) -> &'static str {
    match stage {
        "prepare" => t!("launcher.new_project.progress_prepare_files"),
        "clean" => t!("launcher.new_project.progress_clean_folder"),
        "write" => t!("launcher.new_project.progress_write_png"),
        "browser" => t!("launcher.new_project.progress_start_browser"),
        "collect" => t!("launcher.new_project.progress_search_links"),
        "collect_canvas" => t!("launcher.new_project.progress_collect_canvas"),
        "connect" => t!("launcher.new_project.progress_check_availability"),
        "scan" => t!("launcher.new_project.progress_search_source"),
        "parse_html" => t!("launcher.new_project.progress_parse_html"),
        "archive" => t!("launcher.new_project.progress_extract_archive"),
        "download_waifu2x" => t!("launcher.new_project.progress_download_waifu2x"),
        "verify_waifu2x" => t!("launcher.new_project.progress_verify_waifu2x"),
        "download_waifu2x_retry" => t!("launcher.new_project.progress_download_waifu2x_retry"),
        "extract_waifu2x" => t!("launcher.new_project.progress_extract_waifu2x"),
        "download" => t!("launcher.new_project.progress_download_pages"),
        "save_canvas" => t!("launcher.new_project.progress_save_canvas"),
        "decode" => t!("launcher.new_project.progress_read_images"),
        "filter" => t!("launcher.new_project.progress_filter_width"),
        "preview" => t!("launcher.new_project.progress_prepare_preview"),
        "normalize" => t!("launcher.new_project.progress_align_width"),
        "stitch" => t!("launcher.new_project.progress_stitch_ribbon"),
        "waifu2x" => t!("launcher.new_project.progress_waifu2x"),
        "reline" => t!("launcher.new_project.progress_reline"),
        "cuts" => t!("launcher.new_project.progress_find_cut_lines"),
        "compose" => t!("launcher.new_project.progress_assemble_ribbon"),
        "split" => t!("launcher.new_project.progress_cut_pages"),
        _ => t!("launcher.new_project.progress_processing"),
    }
}

/// The repaint callback a file dialog calls when it resolves, so the frame that polls it runs.
pub(super) fn repaint_waker(ctx: &egui::Context) -> file_dialog::Waker {
    let ctx = ctx.clone();
    Arc::new(move || ctx.request_repaint())
}

pub(super) fn stitch_kind_name(kind: &StitchSuccessKind) -> &'static str {
    match kind {
        StitchSuccessKind::AutoCut => "auto_cut",
        StitchSuccessKind::ReferenceCut => "reference_cut",
        StitchSuccessKind::ManualPreview => "manual_preview",
        StitchSuccessKind::ManualApply => "manual_apply",
        StitchSuccessKind::StitchOnly => "stitch_only",
        StitchSuccessKind::HeterogeneousBottoms => "heterogeneous_bottoms",
    }
}

pub(super) fn stitch_mode_initial_stage(mode: StitchSplitMode) -> &'static str {
    match mode {
        StitchSplitMode::StitchOnly
        | StitchSplitMode::ManualCutPreview
        | StitchSplitMode::HeterogeneousBottoms => "stitch",
        StitchSplitMode::AutoCut => "cut",
    }
}

pub(super) fn stitch_mode_start_status(mode: StitchSplitMode) -> &'static str {
    match mode {
        StitchSplitMode::StitchOnly => t!("launcher.new_project.stitching_progress"),
        StitchSplitMode::ManualCutPreview => t!("launcher.new_project.stitching_manual_progress"),
        StitchSplitMode::AutoCut => t!("launcher.new_project.stitching_auto_progress"),
        StitchSplitMode::HeterogeneousBottoms => t!("launcher.new_project.stitching_uneven_progress"),
    }
}
