/*
File: crates/ms-launcher/src/new_project/window/save.rs

Purpose:
Save section of the New Project window: the save form, the projects-root catalog selection
helpers, and the save flow (target resolution, overwrite confirmation, launch).

Key items:
- `show_save_section`.
- `start_save_to_project`, `start_save_alt_version`, `start_save_to_folder` /
  `poll_save_folder_dialog`.
- `confirm_then_save` -> `show_overwrite_confirm` -> `launch_save` -> `begin_save`.
- `needs_overwrite_confirm`.

Notes:
A non-empty destination is confirmed through the in-app `ConfirmModal` before writing; the
save itself runs in `ProjectSaveController` and is polled in `jobs.rs` (`poll_save`).
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn show_save_section(&mut self, ui: &mut Ui) {
        let titles = self.project_catalog_snapshot.titles.clone();
        let alt_chapters = self.current_alt_chapter_options().to_vec();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.save.is_loading();
        let can_save = self.can_start_save();

        ui.vertical(|ui| {
            let mut project_base = self.save_mode == SaveMode::ProjectBase;
            if ui
                .checkbox(&mut project_base, t!("launcher.new_project.save_as_project_base_button"))
                .changed()
                && project_base
            {
                self.save_mode = SaveMode::ProjectBase;
            }

            let mut alt_version = self.save_mode == SaveMode::AltVersion;
            if ui
                .checkbox(&mut alt_version, t!("launcher.new_project.save_as_alt_version_button"))
                .changed()
                && alt_version
            {
                self.save_mode = SaveMode::AltVersion;
            }

            let mut independent = self.save_mode == SaveMode::Independent;
            if ui
                .checkbox(&mut independent, t!("launcher.new_project.save_independent_button"))
                .changed()
                && independent
            {
                self.save_mode = SaveMode::Independent;
            }
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new(tf!("launcher.new_project.projects_folder_label", arg = self.project_catalog.projects_root().display()))
            .small()
            .weak(),
        );
        if self.project_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.refreshing_titles_status"))
                    .small()
                    .weak(),
            );
        } else if let Some(error) = &self.project_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
        }

        if self.save_mode == SaveMode::ProjectBase {
            ui.add_space(10.0);
            sub_group(
                ui,
                t!("launcher.new_project.save_as_project_base_button"),
                |ui| {
                    field_label(ui, t!("launcher.common.title_label"));
                    let save_title_response = self.save_title_combo.draw(
                        ui,
                        &mut self.save_title_input,
                        titles.as_slice(),
                    );
                    if save_title_response.changed {
                        self.sync_save_title_from_input();
                    }
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    field_label(ui, t!("launcher.new_project.chapter_name_label"));
                    ui.add(
                        TextEdit::singleline(&mut self.save_chapter).desired_width(fill_width(ui)),
                    );
                    if button_sized(
                        ui,
                        t!("launcher.common.save_and_open_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_project(true, false);
                    }
                    if button_sized(
                        ui,
                        t!("launcher.new_project.save_to_project_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_to_project(false, false);
                    }
                },
            );
        }

        if self.save_mode == SaveMode::AltVersion {
            ui.add_space(10.0);
            sub_group(
                ui,
                t!("launcher.new_project.save_as_alt_version_button"),
                |ui| {
                    field_label(ui, t!("launcher.common.title_label"));
                    let previous_alt_title = self.alt_title;
                    combo_index_owned(
                        ui,
                        "launcher_new_project_alt_title",
                        titles.as_slice(),
                        &mut self.alt_title,
                    );
                    if previous_alt_title != self.alt_title {
                        self.alt_chapter = 0;
                    }
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, t!("launcher.common.chapter_label"));
                    combo_index_owned(
                        ui,
                        "launcher_new_project_alt_chapter",
                        alt_chapters.as_slice(),
                        &mut self.alt_chapter,
                    );
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }

                    field_label(ui, t!("launcher.new_project.alt_version_name_label"));
                    ui.add(TextEdit::singleline(&mut self.alt_name).desired_width(fill_width(ui)));
                    if button_sized(
                        ui,
                        t!("launcher.new_project.save_as_alt_version_short_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                        can_save,
                    )
                    .clicked()
                    {
                        self.start_save_alt_version();
                    }
                },
            );
        }

        if self.save_mode == SaveMode::Independent {
            ui.add_space(10.0);
            sub_group(ui, t!("launcher.new_project.save_independent_button"), |ui| {
                if button_sized(
                    ui,
                    t!("launcher.new_project.save_to_folder_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                    can_save,
                )
                .clicked()
                {
                    self.start_save_to_folder(ui.ctx());
                }
            });
        }
        self.show_operation_progress(ui, "save");
    }

    pub(super) fn refresh_project_catalog(&mut self) {
        self.project_catalog_error = None;
        self.project_catalog.refresh();
    }

    pub(super) fn sync_save_title_from_input(&mut self) {
        if let Some(index) = self
            .project_catalog_snapshot
            .titles
            .iter()
            .position(|title| title == self.save_title_input.trim())
        {
            self.save_title = index;
        }
    }

    pub(super) fn current_alt_title_name(&self) -> Option<&str> {
        self.project_catalog_snapshot
            .titles
            .get(self.alt_title)
            .map(String::as_str)
    }

    pub(super) fn current_cut_title_name(&self) -> Option<&str> {
        self.project_catalog_snapshot
            .titles
            .get(self.cut_title)
            .map(String::as_str)
    }

    pub(super) fn current_cut_chapter_options(&self) -> &[String] {
        self.current_cut_title_name()
            .map(|title| chapters_for_title(&self.project_catalog_snapshot, title))
            .unwrap_or(&[])
    }

    pub(super) fn current_cut_chapter_name(&self) -> Option<&str> {
        self.current_cut_chapter_options()
            .get(self.cut_chapter)
            .map(String::as_str)
    }

    pub(super) fn current_alt_chapter_options(&self) -> &[String] {
        self.current_alt_title_name()
            .map(|title| chapters_for_title(&self.project_catalog_snapshot, title))
            .unwrap_or(&[])
    }

    pub(super) fn current_alt_chapter_name(&self) -> Option<&str> {
        self.current_alt_chapter_options()
            .get(self.alt_chapter)
            .map(String::as_str)
    }

    pub(super) fn clamp_project_catalog_indexes(&mut self) {
        if self.save_title >= self.project_catalog_snapshot.titles.len() {
            self.save_title = 0;
        }
        if self.alt_title >= self.project_catalog_snapshot.titles.len() {
            self.alt_title = 0;
        }
        if self.cut_title >= self.project_catalog_snapshot.titles.len() {
            self.cut_title = 0;
        }
        let cut_chapters_len = self.current_cut_chapter_options().len();
        if self.cut_chapter >= cut_chapters_len {
            self.cut_chapter = 0;
        }
        let alt_chapters_len = self.current_alt_chapter_options().len();
        if self.alt_chapter >= alt_chapters_len {
            self.alt_chapter = 0;
        }
    }

    pub(super) fn can_start_save(&self) -> bool {
        !self.source_import.is_loading()
            && !self.project_catalog.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && self.save_folder_dialog.is_none()
            && self.overwrite_confirm.is_none()
            && !self.ribbon.pages().is_empty()
    }

    /// Saves the ribbon into the `src` folder of the chosen title/chapter, asking first when
    /// that folder already has files. `return_to_import` switches back to the import panel
    /// after a successful save ("save and download more").
    pub(super) fn start_save_to_project(&mut self, open_after_save: bool, return_to_import: bool) {
        let title = self.save_title_input.trim().to_string();
        let chapter = self.save_chapter.trim().to_string();
        if title.is_empty() || chapter.is_empty() {
            self.last_error = Some(t!("launcher.new_project.specify_title_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.save_to_project_unavailable").to_string();
            return;
        }
        let target_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::SRC_DIR);
        let needs_confirm = match needs_overwrite_confirm(&target_dir) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_project_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_to_project_failed").to_string();
                ms_log::runtime_log::log_error(format!(
                    "[new-project] failed to inspect project save dir '{}': {err}",
                    target_dir.display()
                ));
                return;
            }
        };
        self.confirm_then_save(
            needs_confirm,
            SaveLaunch {
                target: ProjectSaveTarget::ProjectSource { title, chapter },
                open_after_save,
                return_to_import,
                status_message: t!("launcher.new_project.saving_to_project_progress").to_string(),
            },
        );
    }

    pub(super) fn start_save_alt_version(&mut self) {
        let Some(title) = self.current_alt_title_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_title_for_alt_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        };
        let Some(chapter) = self.current_alt_chapter_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_chapter_for_alt_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        };
        let alt_name = self.alt_name.trim().to_string();
        if alt_name.is_empty() {
            self.last_error = Some(t!("launcher.new_project.specify_alt_name_error").to_string());
            self.import_status = t!("launcher.new_project.save_alt_unavailable").to_string();
            return;
        }
        let target_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::ALT_VERS_DIR)
            .join(&alt_name);
        let needs_confirm = match needs_overwrite_confirm(&target_dir) {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_alt_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_alt_failed").to_string();
                ms_log::runtime_log::log_error(format!(
                    "[new-project] failed to inspect alt save dir '{}': {err}",
                    target_dir.display()
                ));
                return;
            }
        };
        self.confirm_then_save(
            needs_confirm,
            SaveLaunch {
                target: ProjectSaveTarget::AltVersion {
                    title,
                    chapter,
                    alt_name,
                },
                open_after_save: false,
                return_to_import: false,
                status_message: t!("launcher.new_project.saving_alt_progress").to_string(),
            },
        );
    }

    /// Opens the destination-folder picker without blocking; `poll_save_folder_dialog`
    /// continues the save once a folder is chosen. The picker reopens one level above the
    /// previous pick: after saving a chapter into `.../16`, it starts at the title folder so
    /// sibling chapters are one click away (desktop only; other platforms ignore it).
    pub(super) fn start_save_to_folder(&mut self, ctx: &egui::Context) {
        if self.save_folder_dialog.is_some() {
            return;
        }
        let mut request = FileDialogRequest::new(DialogKind::SaveIntoFolder);
        if let Some(parent) = self.last_independent_save_dir.as_deref().and_then(Path::parent) {
            request = request.start_dir(parent);
        }
        self.save_folder_dialog = Some(file_dialog::request(request, Some(repaint_waker(ctx))));
    }

    /// Applies the destination-folder picker's answer: asks before overwriting a non-empty
    /// folder, then starts the save. A cancel changes nothing; a dialog failure is reported.
    pub(super) fn poll_save_folder_dialog(&mut self) {
        let Some(pending) = self.save_folder_dialog.as_mut() else {
            return;
        };
        let Some(result) = pending.poll() else {
            return;
        };
        self.save_folder_dialog = None;
        let target = match result {
            Ok(Some(picked)) => picked.into_folder_target(),
            Ok(None) => return,
            Err(err) => {
                self.report_dialog_error("save folder", &err);
                self.import_status = t!("launcher.new_project.save_to_folder_failed").to_string();
                return;
            }
        };
        let Some(target) = target else {
            // `SaveIntoFolder` always answers with `Picked::FolderTarget` (ms-platform contract).
            self.report_dialog_error("save folder", &FileDialogError::WorkerLost);
            self.import_status = t!("launcher.new_project.save_to_folder_failed").to_string();
            return;
        };
        let folder = target.write_dir().to_path_buf();
        self.last_independent_save_dir = Some(folder.clone());
        let needs_confirm = match target.has_entries() {
            Ok(value) => value,
            Err(err) => {
                self.last_error =
                    Some(t!("launcher.new_project.check_selected_folder_error").to_string());
                self.import_status = t!("launcher.new_project.save_to_folder_failed").to_string();
                ms_log::runtime_log::log_error(format!(
                    "[new-project] failed to inspect folder save dir '{}': {err:?}",
                    folder.display()
                ));
                return;
            }
        };
        self.confirm_then_save(
            needs_confirm,
            SaveLaunch {
                status_message: tf!("launcher.new_project.saving_to_folder_progress", folder = folder.display()),
                target: ProjectSaveTarget::Folder { target },
                open_after_save: false,
                return_to_import: false,
            },
        );
    }

    /// Starts `launch` now, or parks it behind the overwrite confirmation when
    /// `needs_confirm` (the destination folder already has files).
    pub(super) fn confirm_then_save(&mut self, needs_confirm: bool, launch: SaveLaunch) {
        if !needs_confirm {
            self.launch_save(launch);
            return;
        }
        self.overwrite_confirm = Some(PendingOverwrite {
            modal: ConfirmModal::new(
                "launcher.new_project.overwrite_folder_title",
                t!("launcher.new_project.overwrite_folder_title"),
                t!("launcher.new_project.folder_not_empty_prompt"),
            ),
            launch,
        });
    }

    /// Draws the pending overwrite question; "Yes" starts the parked save, "No" drops it.
    pub(super) fn show_overwrite_confirm(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.overwrite_confirm else {
            return;
        };
        match pending.modal.show(ctx) {
            ConfirmAnswer::Pending => {}
            ConfirmAnswer::No => self.overwrite_confirm = None,
            ConfirmAnswer::Yes => {
                if let Some(pending) = self.overwrite_confirm.take() {
                    self.launch_save(pending.launch);
                }
            }
        }
    }

    /// Starts the save worker for a settled destination. The ribbon images are taken now, so a
    /// save confirmed later writes the ribbon as it is at confirmation time.
    pub(super) fn launch_save(&mut self, launch: SaveLaunch) {
        let SaveLaunch { target, open_after_save, return_to_import, status_message } = launch;
        self.begin_save(
            ProjectSaveRequest { target, images: self.current_save_images() },
            open_after_save,
            &status_message,
        );
        // Only a save that actually started may route back to the import panel when it ends.
        if return_to_import && self.save.is_loading() {
            self.return_to_import_after_save_requested = true;
        }
    }

    pub(super) fn begin_save(
        &mut self,
        request: ProjectSaveRequest,
        open_after_save: bool,
        status_message: &str,
    ) {
        if request.images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.no_images_to_save_error").to_string());
            self.import_status = t!("launcher.new_project.save_empty_ribbon_error").to_string();
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "save",
            stage: "prepare".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = status_message.to_string();
        self.pending_open_selection = None;
        self.open_after_save_requested = open_after_save;
        self.save.begin(request);
    }
}

/// Whether saving into `dir` must first ask before overwriting: only an existing directory
/// that already has entries does. A missing directory is created by the save.
///
/// # Errors
/// The I/O error of listing an existing `dir`.
pub(super) fn needs_overwrite_confirm(dir: &Path) -> std::io::Result<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    dir_has_entries(dir)
}
