/*
File: crates/ms-launcher/src/new_project/window/processing_ui.rs

Purpose:
Image-processing sections of the New Project window: stitch/split controls, the processor
picker, waifu2x settings and the Reline section (simplified gallery and full/expert form) with
its model catalog combo.

Key items:
- `show_stitch_section`, `show_image_processing_section`, `show_waifu_section`.
- `show_reline_section` -> `show_reline_simple` / `show_reline_full`, `show_reline_model_gallery`.
- `ensure_reline_model_catalog_requested`, `refresh_reline_model_catalog`, `show_reline_model_combo`.

Notes:
Option strings typed here are parsed in `options.rs`; runs start in `jobs.rs`.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn show_stitch_section(&mut self, ui: &mut Ui) {
        let cut_titles = self.project_catalog_snapshot.titles.clone();
        let cut_chapters = self.current_cut_chapter_options().to_vec();
        let can_start = self.can_start_stitch();
        let can_restore = self.ribbon.can_restore_original() || self.has_manual_cut_preview();
        let can_apply_manual = self.has_manual_cut_preview() && !self.stitch.is_loading();
        let can_refresh_catalog = !self.project_catalog.is_loading() && !self.stitch.is_loading();
        let can_take_chapter = self.cut_as_chapter_enabled
            && !self.stitch.is_loading()
            && self.current_cut_title_name().is_some()
            && self.current_cut_chapter_name().is_some()
            && !self.ribbon.pages().is_empty();
        let can_pick_folder = self.cut_as_chapter_enabled
            && !self.stitch.is_loading()
            && self.cut_like_folder_dialog.is_none()
            && !self.ribbon.pages().is_empty();

        field_label(ui, t!("launcher.new_project.stitch_k_label"));
        ui.add(
            TextEdit::singleline(&mut self.stitch_parts)
                .desired_width(160.0)
                .hint_text(t!("launcher.new_project.stitch_k_placeholder")),
        );

        field_label(ui, t!("launcher.new_project.stitch_hmax_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_target_height).desired_width(160.0));

        field_label(ui, t!("launcher.new_project.stitch_band_rows_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_band_rows).desired_width(160.0));

        field_label(ui, t!("launcher.new_project.stitch_tol_label"));
        ui.add(TextEdit::singleline(&mut self.stitch_tolerance).desired_width(160.0));

        field_label(ui, "search_radius (px)");
        ui.add(TextEdit::singleline(&mut self.stitch_search_radius).desired_width(160.0));

        ui.checkbox(&mut self.stitch_prefer_up, t!("launcher.new_project.stitch_up_first_check"));

        let wide_button = egui::vec2(LEFT_PANEL_WIDTH - 52.0, 32.0);
        if button_sized(ui, t!("launcher.new_project.stitch_ribbon_button"), wide_button, can_start).clicked() {
            self.start_stitch_split(StitchSplitMode::StitchOnly);
        }
        if button_sized(ui, t!("launcher.new_project.stitch_set_cut_lines_button"), wide_button, can_start).clicked()
        {
            self.start_stitch_split(StitchSplitMode::ManualCutPreview);
        }
        let stitch_auto = button_sized(ui, t!("launcher.new_project.stitch_cut_auto_button"), wide_button, can_start);
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_STITCH, stitch_auto.rect);
        if stitch_auto.clicked() {
            self.start_stitch_split(StitchSplitMode::AutoCut);
        }
        if button_sized(
            ui,
            t!("launcher.new_project.stitch_only_uneven_button"),
            wide_button,
            can_start,
        )
        .clicked()
        {
            self.start_stitch_split(StitchSplitMode::HeterogeneousBottoms);
        }
        if button_sized(ui, t!("launcher.new_project.restore_original_button"), ACTION_BUTTON_SIZE, can_restore).clicked()
        {
            self.restore_original_pages();
        }
        if can_apply_manual
            && button_sized(ui, t!("launcher.new_project.apply_manual_cut_button"), wide_button, true).clicked()
        {
            self.apply_manual_cut_guides();
        }

        ui.add_space(8.0);
        self.show_operation_progress(ui, "stitch");
        sub_group(ui, t!("launcher.new_project.cut_as_chapter_button"), |ui| {
            ui.checkbox(
                &mut self.cut_as_chapter_enabled,
                t!("launcher.new_project.enable_cut_by_chapter_check"),
            );
            ui.add_enabled_ui(self.cut_as_chapter_enabled, |ui| {
                field_label(ui, t!("launcher.common.title_label"));
                let previous_cut_title = self.cut_title;
                combo_index_owned(
                    ui,
                    "launcher_new_project_cut_title",
                    cut_titles.as_slice(),
                    &mut self.cut_title,
                );
                if previous_cut_title != self.cut_title {
                    self.cut_chapter = 0;
                }
                field_label(ui, t!("launcher.common.chapter_label"));
                combo_index_owned(
                    ui,
                    "launcher_new_project_cut_chapter",
                    cut_chapters.as_slice(),
                    &mut self.cut_chapter,
                );
                ui.horizontal(|ui| {
                    if button_sized(ui, t!("launcher.common.refresh_button"), SMALL_BUTTON_SIZE, can_refresh_catalog)
                        .clicked()
                    {
                        self.refresh_project_catalog();
                    }
                    if button_sized(ui, t!("launcher.new_project.take_this_chapter_button"), ACTION_BUTTON_SIZE, can_take_chapter)
                        .clicked()
                    {
                        self.start_cut_like_project_chapter();
                    }
                });
                if button_sized(
                    ui,
                    t!("launcher.new_project.choose_folder_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 84.0, 32.0),
                    can_pick_folder,
                )
                .clicked()
                {
                    self.start_cut_like_folder(ui.ctx());
                }
            });
        });
    }

    pub(super) fn show_image_processing_section(&mut self, ui: &mut Ui) {
        let processor_labels = ["waifu2x", "Reline"];
        let mut processor_index = self.image_processor.as_index();
        field_label(ui, t!("launcher.new_project.waifu2x_engine_label"));
        combo_index(
            ui,
            "launcher_new_project_image_processor",
            &processor_labels,
            &mut processor_index,
        );
        self.image_processor = ImageProcessor::from_index(processor_index);
        ui.add_space(8.0);
        match self.image_processor {
            ImageProcessor::Waifu2x => self.show_waifu_section(ui),
            ImageProcessor::Reline => self.show_reline_section(ui),
        }
    }

    pub(super) fn show_waifu_section(&mut self, ui: &mut Ui) {
        let noise_levels = ["-1", "0", "1", "2", "3"];
        let scale_levels = ["1", "2", "4", "8", "16", "32"];

        field_label(ui, t!("launcher.new_project.waifu2x_backend_path_label"));
        let mut backend = self.waifu_backend_path_display();
        ui.add_enabled(
            false,
            TextEdit::singleline(&mut backend).desired_width(fill_width(ui)),
        );
        if let Some(reason) = self.waifu2x.unavailable_reason() {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), reason);
        }

        field_label(ui, t!("launcher.new_project.waifu2x_noise_label"));
        combo_index(
            ui,
            "launcher_new_project_w2x_noise",
            &noise_levels,
            &mut self.waifu_noise,
        );

        field_label(ui, t!("launcher.new_project.waifu2x_scale_label"));
        combo_index(
            ui,
            "launcher_new_project_w2x_scale",
            &scale_levels,
            &mut self.waifu_scale,
        );

        field_label(ui, "Tile size -t (>=32, 0=auto)");
        ui.add(TextEdit::singleline(&mut self.waifu_tile_size).desired_width(160.0));

        let waifu_run = button_sized(
            ui,
            t!("launcher.new_project.run_waifu2x_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_waifu2x(),
        );
        #[cfg(feature = "tutorial")]
        self.tutorial.mark(tutorial::TARGET_WAIFU, waifu_run.rect);
        if waifu_run.clicked() {
            self.start_waifu2x();
        }
        self.show_operation_progress(ui, "waifu2x");
    }

    /// Reline section entry point: draws the simple/full toggle and dispatches to the active mode.
    ///
    /// The mode is persisted in user config; toggling writes the new mode once (not per frame).
    pub(super) fn show_reline_section(&mut self, ui: &mut Ui) {
        let mut mode_index = self.reline_ui_mode.as_index();
        field_label(ui, t!("launcher.new_project.reline_interface_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_ui_mode",
            &[t!("launcher.new_project.reline_interface_simple"), t!("launcher.new_project.reline_interface_full")],
            &mut mode_index,
        );
        let new_mode = RelineUiMode::from_index(mode_index);
        if new_mode != self.reline_ui_mode {
            self.reline_ui_mode = new_mode;
            save_reline_ui_mode(new_mode);
        }
        ui.add_space(8.0);

        match self.reline_ui_mode {
            RelineUiMode::Simple => self.show_reline_simple(ui),
            RelineUiMode::Full => self.show_reline_full(ui),
        }
    }

    /// Guided Reline UI: a categorized model gallery plus a small set of high-level controls.
    ///
    /// Hidden parameters take safe defaults; the resulting `RelineOptions` are built by
    /// `build_reline_simple_options`.
    pub(super) fn show_reline_simple(&mut self, ui: &mut Ui) {
        self.ensure_reline_model_catalog_requested();

        field_label(ui, t!("launcher.new_project.reline_model_label"));
        self.show_reline_model_gallery(ui);
        ui.add_space(10.0);

        sub_group(ui, t!("launcher.new_project.reline_process_mode_label"), |ui| {
            for preset in RelineSimplePreset::ALL {
                let mut selected = self.reline_simple_preset == preset.as_index();
                if ui.radio(selected, preset.label()).clicked() {
                    selected = true;
                }
                if selected {
                    self.reline_simple_preset = preset.as_index();
                }
            }
            let active = RelineSimplePreset::from_index(self.reline_simple_preset);
            ui.add_space(4.0);
            ui.label(RichText::new(active.hint()).small().weak());
        });
        ui.add_space(8.0);

        field_label(ui, t!("launcher.new_project.reline_sharpness_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_simple_sharp",
            &[t!("launcher.new_project.reline_sharpness_none"), t!("launcher.new_project.reline_sharpness_weak"), t!("launcher.new_project.reline_sharpness_strong")],
            &mut self.reline_simple_sharp,
        );

        field_label(ui, t!("launcher.new_project.reline_target_scale_label"));
        combo_index(
            ui,
            "launcher_new_project_reline_simple_scale",
            &[t!("launcher.new_project.reline_scale_auto"), "×2", "×4"],
            &mut self.reline_simple_scale,
        );

        ui.add_space(4.0);
        ui.checkbox(
            &mut self.reline_simple_resize_enabled,
            t!("launcher.new_project.reline_result_height_label"),
        );
        if self.reline_simple_resize_enabled {
            ui.add(
                TextEdit::singleline(&mut self.reline_simple_resize_target).desired_width(160.0),
            );
        }

        ui.add_space(10.0);
        if button_sized(
            ui,
            t!("launcher.new_project.run_reline_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_reline(),
        )
        .clicked()
        {
            self.start_reline();
        }
        self.show_operation_progress(ui, "reline");
    }

    /// Categorized model picker used by the simplified Reline UI.
    ///
    /// Joins the fetched catalog (`reline_model_catalog_entries`) with offline classification
    /// from `reline_models::classify`. The full categorized list lives inside a collapsible area
    /// toggled by the "Выбрать модель" button; when collapsed only the selected model's card is
    /// shown. The expanded list grows inline (no inner scroll) — the surrounding page scrolls.
    pub(super) fn show_reline_model_gallery(&mut self, ui: &mut Ui) {
        use crate::new_project::reline_models::{ModelCategory, classify};

        if self.reline_model_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_loading_models_status"))
                    .small()
                    .weak(),
            );
        }
        if let Some(error) = &self.reline_model_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), error.clone());
        }

        ui.horizontal(|ui| {
            let toggle_label = if self.reline_model_picker_open {
                t!("launcher.new_project.reline_hide_list_button")
            } else {
                t!("launcher.new_project.reline_choose_model_button")
            };
            if ui.button(toggle_label).clicked() {
                self.reline_model_picker_open = !self.reline_model_picker_open;
            }
            let refresh_label = if self.reline_model_catalog.is_loading() {
                t!("launcher.common.loading")
            } else {
                t!("launcher.common.refresh_button")
            };
            if ui
                .add_enabled(
                    !self.reline_model_catalog.is_loading(),
                    Button::new(refresh_label),
                )
                .on_hover_text(t!("launcher.new_project.reline_refresh_models_tooltip"))
                .clicked()
            {
                self.refresh_reline_model_catalog();
            }
        });

        if self.reline_model_catalog_entries.is_empty() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_models_empty_hint"))
                    .small()
                    .weak(),
            );
            return;
        }

        // Collapsed: show only the currently selected model's title and description.
        if !self.reline_model_picker_open {
            if self.reline_model_name.trim().is_empty() {
                ui.label(
                    RichText::new(t!("launcher.new_project.reline_no_model_selected_hint"))
                        .small()
                        .weak(),
                );
            } else {
                let meta = classify(&self.reline_model_name);
                ui.label(RichText::new(meta.display_title()).strong());
                ui.label(RichText::new(&self.reline_model_name).small().weak());
                ui.label(RichText::new(meta.description).small());
            }
            return;
        }

        // Expanded: full categorized list, grown inline (no inner ScrollArea).
        // Group entries by classified category, preserving catalog order within each group.
        let classified: Vec<(usize, ModelCategory)> = self
            .reline_model_catalog_entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, classify(&entry.name).category))
            .collect();

        for category in ModelCategory::ALL {
            let group: Vec<usize> = classified
                .iter()
                .filter(|(_, cat)| *cat == category)
                .map(|(index, _)| *index)
                .collect();
            if group.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(format!("{} ({})", category.title(), group.len()))
                .default_open(category.order() == 0)
                .id_salt(("reline_gallery", category.order()))
                .show(ui, |ui| {
                    for index in group {
                        let entry = &self.reline_model_catalog_entries[index];
                        let name = entry.name.clone();
                        let downloaded = entry.downloaded;
                        let meta = classify(&name);
                        let selected = self.reline_model_name == name;

                        let header = if downloaded {
                            tf!("launcher.new_project.reline_model_downloaded_marker", meta = meta.display_title())
                        } else {
                            meta.display_title()
                        };
                        let response =
                            ui.selectable_label(selected, RichText::new(header).strong());
                        ui.label(RichText::new(&name).small().weak());
                        ui.label(RichText::new(&meta.description).small());
                        if let Some(recommendation) = &meta.recommendation {
                            ui.label(
                                RichText::new(recommendation)
                                    .small()
                                    .color(egui::Color32::from_rgb(120, 190, 120)),
                            );
                        }
                        if response.clicked() {
                            // Pick the model and collapse the list back to the compact card.
                            self.reline_model_name = name;
                            self.reline_model_picker_open = false;
                        }
                        ui.add_space(6.0);
                    }
                });
        }
    }

    /// Full (expert) Reline UI: every pipeline node with raw parameters. Kept behind the toggle.
    pub(super) fn show_reline_full(&mut self, ui: &mut Ui) {
        const READER_MODES: [&str; 3] = ["rgb", "gray", "dynamic"];
        const TILERS: [&str; 3] = ["exact", "max", "no_tiling"];
        const DTYPES: [&str; 3] = ["F32", "F16", "BF16"];
        const CANNY_TYPES: [&str; 3] = ["invert", "normal", "unsharp"];
        const DOT_TYPES: [&str; 5] = ["line", "cross", "ellipse", "invline", "circle"];
        const HALFTONE_MODES: [&str; 4] = ["gray", "rgb", "hsv", "cmyk"];
        const HALFTONE_FILTERS: [&str; 22] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "linear",
            "slinear4",
            "slinear8",
            "hamming",
            "shamming4",
            "shamming8",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "gauss",
            "sgauss4",
            "sgauss8",
        ];
        const RESIZE_FILTERS: [&str; 33] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "ibox",
            "linear",
            "slinear4",
            "slinear8",
            "ilinear",
            "hamming",
            "shamming4",
            "shamming8",
            "ihamming",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "icatmullrom",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "imitchell",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "ilanczos",
            "gauss",
            "sgauss4",
            "sgauss8",
            "igauss",
            "dpid_0.25",
            "dpid_0.5",
            "dpid_0.75",
            "dpid_1",
        ];
        const CVT_TYPES: [&str; 4] = ["RGB2Gray2020", "RGB2Gray709", "RGB2Gray", "Gray2RGB"];

        self.ensure_reline_model_catalog_requested();

        field_label_hover(ui, t!("launcher.new_project.reline_read_mode_label"), t!("launcher.new_project.reline_read_mode_tooltip"));
        combo_index(
            ui,
            "launcher_new_project_reline_reader_mode",
            &READER_MODES,
            &mut self.reline_reader_mode,
        );

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_restore")).id_salt("launcher.new_project.reline_section_restore")
            .default_open(true)
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.reline_upscale_enabled,
                    t!("launcher.new_project.reline_enable_restore"),
                );
                field_label(ui, t!("launcher.new_project.reline_model_from_catalog"));
                self.show_reline_model_combo(ui);
                field_label(ui, t!("launcher.new_project.reline_local_model_path"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_model_path).desired_width(fill_width(ui)),
                );
                field_label(ui, t!("launcher.new_project.reline_direct_url"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_model_url).desired_width(fill_width(ui)),
                );
                field_label(ui, t!("launcher.new_project.reline_tiling"));
                combo_index(
                    ui,
                    "launcher_new_project_reline_tiler",
                    &TILERS,
                    &mut self.reline_tiler,
                );
                field_label(ui, t!("launcher.new_project.reline_target_scale_field_hint"));
                ui.add(TextEdit::singleline(&mut self.reline_target_scale).desired_width(160.0));
                field_label(ui, t!("launcher.new_project.reline_compute_type"));
                combo_index(
                    ui,
                    "launcher_new_project_reline_dtype",
                    &DTYPES,
                    &mut self.reline_dtype,
                );
                field_label(ui, t!("launcher.new_project.reline_exact_tile_size"));
                ui.add(
                    TextEdit::singleline(&mut self.reline_exact_tiler_size).desired_width(160.0),
                );
                ui.checkbox(
                    &mut self.reline_allow_cpu_upscale,
                    t!("launcher.new_project.reline_allow_cpu"),
                );
            });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_restore_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_sharpen")).id_salt("launcher.new_project.reline_section_sharpen").show(ui, |ui| {
            ui.checkbox(&mut self.reline_sharp_enabled, t!("launcher.new_project.reline_enable_sharpen"));
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_low_level"),
                &mut self.reline_sharp_low_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_high_level"),
                &mut self.reline_sharp_high_input,
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_gamma"), &mut self.reline_sharp_gamma);
            numeric_text_field(ui, t!("launcher.new_project.reline_white_range"), &mut self.reline_sharp_diapason_white);
            numeric_text_field(ui, t!("launcher.new_project.reline_black_range"), &mut self.reline_sharp_diapason_black);
            ui.checkbox(&mut self.reline_sharp_canny, t!("launcher.new_project.reline_canny_contour"));
            field_label(ui, t!("launcher.new_project.reline_canny_mode"));
            combo_index(
                ui,
                "launcher_new_project_reline_canny_type",
                &CANNY_TYPES,
                &mut self.reline_sharp_canny_type,
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_sharpen_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_halftone")).id_salt("launcher.new_project.reline_section_halftone").show(ui, |ui| {
            ui.checkbox(&mut self.reline_halftone_enabled, t!("launcher.new_project.reline_enable_halftone"));
            numeric_text_field(ui, t!("launcher.new_project.reline_dot_size"), &mut self.reline_halftone_dot_size);
            numeric_text_field(ui, t!("launcher.new_project.reline_angle"), &mut self.reline_halftone_angle);
            field_label(ui, t!("launcher.new_project.reline_dot_type"));
            combo_index(
                ui,
                "launcher_new_project_reline_dot_type",
                &DOT_TYPES,
                &mut self.reline_halftone_dot_type,
            );
            field_label(ui, t!("launcher.new_project.reline_halftone_color_mode"));
            combo_index(
                ui,
                "launcher_new_project_reline_halftone_mode",
                &HALFTONE_MODES,
                &mut self.reline_halftone_mode,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_ssaa_scale"),
                &mut self.reline_halftone_ssaa_scale,
            );
            field_label(ui, t!("launcher.new_project.reline_ssaa_filter"));
            combo_index(
                ui,
                "launcher_new_project_reline_halftone_filter",
                &HALFTONE_FILTERS,
                &mut self.reline_halftone_ssaa_filter,
            );
            ui.checkbox(
                &mut self.reline_halftone_disable_auto_dot,
                t!("launcher.new_project.reline_disable_auto_dot"),
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_halftone_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_resize")).id_salt("launcher.new_project.reline_section_resize").show(ui, |ui| {
            ui.checkbox(
                &mut self.reline_resize_enabled,
                t!("launcher.new_project.reline_enable_resize"),
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_height"), &mut self.reline_resize_height);
            numeric_text_field(ui, t!("launcher.new_project.reline_width"), &mut self.reline_resize_width);
            numeric_text_field(ui, t!("launcher.new_project.reline_percent"), &mut self.reline_resize_percent);
            field_label(ui, t!("launcher.new_project.reline_filter"));
            combo_index(
                ui,
                "launcher_new_project_reline_resize_filter",
                &RESIZE_FILTERS,
                &mut self.reline_resize_filter,
            );
            ui.checkbox(&mut self.reline_resize_gamma_correction, t!("launcher.new_project.reline_gamma_correction"));
            ui.checkbox(&mut self.reline_resize_spread, t!("launcher.new_project.reline_spread"));
            numeric_text_field(ui, t!("launcher.new_project.reline_spread_size"), &mut self.reline_resize_spread_size);
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_resize_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_levels")).id_salt("launcher.new_project.reline_section_levels").show(ui, |ui| {
            ui.checkbox(&mut self.reline_level_enabled, t!("launcher.new_project.reline_enable_levels"));
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_low_level"),
                &mut self.reline_level_low_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_input_high_level"),
                &mut self.reline_level_high_input,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_output_low_level"),
                &mut self.reline_level_low_output,
            );
            numeric_text_field(
                ui,
                t!("launcher.new_project.reline_output_high_level"),
                &mut self.reline_level_high_output,
            );
            numeric_text_field(ui, t!("launcher.new_project.reline_gamma"), &mut self.reline_level_gamma);
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_levels_tooltip"));

        let response = egui::CollapsingHeader::new(t!("launcher.new_project.reline_section_colorconv")).id_salt("launcher.new_project.reline_section_colorconv").show(ui, |ui| {
            ui.checkbox(
                &mut self.reline_cvt_color_enabled,
                t!("launcher.new_project.reline_enable_colorconv"),
            );
            field_label(ui, t!("launcher.new_project.reline_conv_type"));
            combo_index(
                ui,
                "launcher_new_project_reline_cvt_type",
                &CVT_TYPES,
                &mut self.reline_cvt_color_type,
            );
        });
        response.header_response.on_hover_text(t!("launcher.new_project.reline_colorconv_tooltip"));

        if button_sized(
            ui,
            t!("launcher.new_project.run_reline_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            self.can_start_reline(),
        )
        .clicked()
        {
            self.start_reline();
        }
        self.show_operation_progress(ui, "reline");
    }

    pub(super) fn ensure_reline_model_catalog_requested(&mut self) {
        if self.reline_model_catalog_requested {
            return;
        }
        self.reline_model_catalog_requested = true;
        self.reline_model_catalog_error = None;
        self.reline_model_catalog.begin();
    }

    pub(super) fn refresh_reline_model_catalog(&mut self) {
        self.reline_model_catalog_requested = true;
        self.reline_model_catalog_error = None;
        self.reline_model_catalog.begin();
    }

    pub(super) fn show_reline_model_combo(&mut self, ui: &mut Ui) {
        let mut options: Vec<(String, String, String)> = self
            .reline_model_catalog_entries
            .iter()
            .map(|entry| {
                let label = if entry.downloaded {
                    tf!("launcher.new_project.reline_entry_downloaded_marker", entry = entry.name)
                } else {
                    entry.name.clone()
                };
                (entry.name.clone(), label, entry.filename.clone())
            })
            .collect();

        let current_model = self.reline_model_name.trim();
        if !current_model.is_empty() && !options.iter().any(|(name, _, _)| name == current_model) {
            options.insert(
                0,
                (
                    current_model.to_string(),
                    tf!("launcher.new_project.reline_current_value_marker", current_model = current_model),
                    String::new(),
                ),
            );
        }

        ui.horizontal(|ui| {
            let refresh_width = 96.0;
            let combo_width =
                (ui.available_width() - refresh_width - ui.spacing().item_spacing.x).max(150.0);
            WheelComboBox::from_id_salt("launcher_new_project_reline_model_name")
                .width(combo_width)
                .selected_text(if self.reline_model_name.trim().is_empty() {
                    t!("launcher.new_project.reline_not_selected")
                } else {
                    self.reline_model_name.as_str()
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.reline_model_name,
                        String::new(),
                        t!("launcher.new_project.reline_dont_pick_from_catalog"),
                    )
                    .on_hover_text(t!("launcher.new_project.reline_use_local_or_url_hint"));
                    if options.is_empty() {
                        ui.label(t!("launcher.new_project.reline_models_not_loaded"));
                    } else {
                        for (name, label, filename) in &options {
                            let response = ui.selectable_value(
                                &mut self.reline_model_name,
                                name.clone(),
                                label,
                            );
                            if !filename.trim().is_empty() {
                                response.on_hover_text(tf!("launcher.new_project.reline_file_label", filename = filename));
                            }
                        }
                    }
                });

            let refresh_label = if self.reline_model_catalog.is_loading() {
                t!("launcher.common.loading")
            } else {
                t!("launcher.common.refresh_button")
            };
            if ui
                .add_enabled(
                    !self.reline_model_catalog.is_loading(),
                    Button::new(refresh_label).min_size(egui::vec2(refresh_width, 28.0)),
                )
                .on_hover_text(t!("launcher.new_project.reline_refresh_models_tooltip"))
                .clicked()
            {
                self.refresh_reline_model_catalog();
            }
        });

        if self.reline_model_catalog.is_loading() {
            ui.label(
                RichText::new(t!("launcher.new_project.reline_loading_models_status"))
                    .small()
                    .weak(),
            );
        }
        if let Some(error) = &self.reline_model_catalog_error {
            ui.colored_label(egui::Color32::from_rgb(236, 112, 99), error);
        }
    }
}

