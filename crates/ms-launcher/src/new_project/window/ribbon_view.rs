/*
File: crates/ms-launcher/src/new_project/window/ribbon_view.rs

Purpose:
Ribbon viewer of the New Project window: the viewer panel, ribbon layout and resize, per-page
controls and edits (remove, swap, merge, crop button), manual cut guides and the cut-like /
stitch-split entry points that act on the ribbon.

Key items:
- `show_viewer_panel` -> `show_ribbon`, `apply_ribbon_resize`, `apply_ribbon_action`.
- Manual cut: `add_manual_cut_guide`, `apply_manual_cut_guides`, `draw_manual_cut_guides`.
- Geometry: `ribbon_scale_base_px`, `ribbon_page_display_size`, `ribbon_page_left_inset`,
  `clamp_ribbon_width_fraction`.

Notes:
Every page rect derives from one `(width_scale, image_size)` pair; page images are painted only
through `ribbon::paint_ribbon_tiles`.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn show_viewer_panel(&mut self, ui: &mut Ui) {
        ui.add_space(2.0);
        Frame::group(ui.style())
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_min_height(VIEWER_MIN_HEIGHT);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t!("launcher.new_project.pages_preview_title")).size(20.0).strong());
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(tf!("launcher.new_project.pages_on_ribbon_count", arg = self.ribbon.pages().len()))
                            .small()
                            .weak(),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.has_manual_cut_preview() {
                                let button = Button::new(
                                    RichText::new(t!("launcher.new_project.cut_button"))
                                        .size(14.0)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(183, 28, 28))
                                .stroke(Stroke::new(1.0, egui::Color32::from_rgb(255, 121, 121)))
                                .corner_radius(999.0);
                                if ui
                                    .add_enabled(
                                        !self.stitch.is_loading(),
                                        button.min_size(MANUAL_CUT_APPLY_BUTTON_SIZE),
                                    )
                                    .clicked()
                                {
                                    self.apply_manual_cut_guides();
                                }
                                ui.add_space(8.0);
                            }
                            if let Some(folder) = self.ribbon.loaded_source() {
                                ui.label(
                                    RichText::new(folder.display().to_string()).small().weak(),
                                );
                            } else {
                                ui.label(RichText::new(t!("launcher.new_project.folder_not_selected")).small().weak());
                            }
                        });
                    });

                    ui.add_space(6.0);
                    ui.label(RichText::new(&self.import_status).small());
                    if let Some(error) = &self.last_error {
                        ui.add_space(4.0);
                        ui.colored_label(egui::Color32::from_rgb(255, 120, 120), error);
                    }

                    ui.add_space(6.0);
                    let uniform_width_before = self.ribbon_uniform_width;
                    ui.checkbox(
                        &mut self.ribbon_uniform_width,
                        t!("launcher.new_project.ribbon_uniform_width_check"),
                    );
                    // Persist on toggle only: the config write touches the filesystem and must
                    // not run once per frame.
                    if self.ribbon_uniform_width != uniform_width_before {
                        save_ribbon_uniform_width(self.ribbon_uniform_width);
                    }

                    ui.add_space(10.0);
                    self.show_ribbon(ui);
                });
            });
    }

    pub(super) fn show_ribbon(&mut self, ui: &mut Ui) {
        let available_height = safe_dimension(ui.available_size_before_wrap().y, VIEWER_MIN_HEIGHT);
        let outer_rect = ui.available_rect_before_wrap();
        // Reserve the whole ribbon area up front so the edge handles interact against a stable
        // region. No space is allocated twice: `scope_builder` ends in `advance_cursor_after_rect`,
        // which SETS the cursor from the child rect instead of incrementing it.
        ui.allocate_rect(outer_rect, egui::Sense::hover());
        let container_width = self.apply_ribbon_resize(ui, outer_rect);
        let container_rect = egui::Rect::from_min_size(
            egui::pos2(
                outer_rect.center().x - container_width * 0.5,
                outer_rect.min.y,
            ),
            egui::vec2(container_width, outer_rect.height()),
        );
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(container_rect)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                Frame::new()
                    .fill(egui::Color32::from_rgba_premultiplied(18, 18, 22, 160))
                    .stroke(Stroke::new(
                        1.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    ))
                    .corner_radius(12.0)
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_min_height((available_height - 8.0).max(220.0));
                        let mut cut_marker_screen_positions = Vec::new();
                        let mut page_boundary_screen_positions = Vec::new();
                        let scroll_output = MarkedScrollArea::vertical("launcher_new_project_ribbon_scroll")
                            .floating(false)
                            .gutter_width(MANUAL_CUT_SCROLL_ARROW_WIDTH)
                            .show(ui, |ui| {
                                if self.source_import.is_loading()
                                    || self.advanced_download.is_loading()
                                    || self.quick_download.is_loading()
                                    || self.test_chapter_check_rx.is_some()
                                    || self.stitch.is_loading()
                                    || self.save.is_loading()
                                    || self.waifu2x.is_loading()
                                    || self.reline.is_loading()
                                {
                                    let progress = self.current_progress(true);
                                    ui.add(
                                        ProgressBar::new(progress.fraction)
                                            .animate(true)
                                            .desired_width(fill_width(ui))
                                            .text(progress.label),
                                    );
                                    return;
                                }

                                if self.ribbon.pages().is_empty() {
                                    ui.add_space(24.0);
                                    ui.vertical_centered(|ui| {
                                        ui.label(RichText::new(t!("launcher.new_project.ribbon_empty_hint")).size(18.0));
                                        ui.add_space(4.0);
                                        ui.label(
                                            RichText::new(
                                                t!("launcher.new_project.ribbon_empty_description"),
                                            )
                                            .small()
                                            .weak(),
                                        );
                                    });
                                    ui.add_space(24.0);
                                    return;
                                }

                                let pages_len = self.ribbon.pages().len();
                                let mut ribbon_action = None;
                                let mut add_manual_cut_at = None;
                                let mut manual_cut_context_guide = self.manual_cut_context_guide;
                                let mut selected_page = self.selected_ribbon_page;
                                let uniform_width = self.ribbon_uniform_width;
                                // Proportional mode measures every page against the widest one,
                                // so this is resolved once instead of per page.
                                // One upload allowance for the whole ribbon per frame, shared
                                // by every page.
                                let mut upload_budget = ribbon_upload_budget();
                                let widest_page_px = self
                                    .ribbon
                                    .pages()
                                    .iter()
                                    .map(|page| page.original_size[0])
                                    .max()
                                    .unwrap_or(1);
                                for index in 0..pages_len {
                                    let mut manual_overlay = None;
                                    ui.vertical(|ui| {
                                        {
                                            let page = &mut self.ribbon.pages_mut()[index];
                                            let content_width = safe_dimension(ui.available_width(), 120.0);
                                            let scale_base_px = ribbon_scale_base_px(
                                                uniform_width,
                                                page.original_size[0],
                                                widest_page_px,
                                            );
                                            let (width_scale, image_size) = ribbon_page_display_size(
                                                content_width,
                                                page.original_size,
                                                scale_base_px,
                                            );
                                            // Narrower-than-full pages are centered; the label
                                            // follows the image's left edge. In uniform mode the
                                            // inset is zero, so this is the previous layout.
                                            let left_inset = ribbon_page_left_inset(content_width, image_size.x);
                                            ui.horizontal(|ui| {
                                                ui.add_space(left_inset);
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{}  {}x{}",
                                                        page.name,
                                                        page.original_size[0],
                                                        page.original_size[1]
                                                    ))
                                                    .small()
                                                    .weak(),
                                                );
                                            });
                                            // The row spans the full content width so the empty
                                            // margin beside a narrow page stays inert; only
                                            // `image_rect` senses clicks.
                                            let row_rect = ui
                                                .allocate_exact_size(
                                                    egui::vec2(content_width, image_size.y),
                                                    egui::Sense::hover(),
                                                )
                                                .0;
                                            let image_rect = egui::Rect::from_min_size(
                                                egui::pos2(row_rect.left() + left_inset, row_rect.top()),
                                                image_size,
                                            );
                                            let image_response = ui.interact(
                                                image_rect,
                                                egui::Id::new(("launcher_new_project_ribbon_page", index)),
                                                egui::Sense::click(),
                                            );
                                            if image_response.clicked() {
                                                selected_page = Some(index);
                                            }
                                            if image_response.secondary_clicked()
                                                && let Some(pointer_pos) = image_response.interact_pointer_pos()
                                            {
                                                let page_height = page.original_size[1].max(1);
                                                let image_y =
                                                    ((pointer_pos.y - image_rect.top()) / width_scale)
                                                        .round()
                                                        .clamp(
                                                            1.0,
                                                            page_height.saturating_sub(1) as f32,
                                                        )
                                                        as usize;
                                                manual_cut_context_guide = Some(ManualCutGuide {
                                                    page_index: index,
                                                    y: image_y,
                                                });
                                            }
                                            image_response.context_menu(|ui| {
                                                if ui.button(t!("launcher.new_project.add_cut_line_menu")).clicked() {
                                                    add_manual_cut_at = manual_cut_context_guide;
                                                    ui.close();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        index > 0,
                                                        Button::new(t!("launcher.new_project.stitch_with_prev_menu")),
                                                    )
                                                    .clicked()
                                                {
                                                    ribbon_action =
                                                        Some((index, RibbonImageControlAction::MergeWithPrevious));
                                                    ui.close();
                                                }
                                                if ui
                                                    .add_enabled(
                                                        index + 1 < pages_len,
                                                        Button::new(t!("launcher.new_project.stitch_with_next_menu")),
                                                    )
                                                    .clicked()
                                                {
                                                    ribbon_action =
                                                        Some((index, RibbonImageControlAction::MergeWithNext));
                                                    ui.close();
                                                }
                                            });
                                            paint_ribbon_tiles(
                                                ui,
                                                image_rect,
                                                width_scale,
                                                &mut page.tiles,
                                                &format!("launcher-new-project-ribbon-{index}"),
                                                &mut upload_budget,
                                            );
                                            if selected_page == Some(index) {
                                                ui.painter().rect_stroke(
                                                    image_rect.expand(2.0),
                                                    10.0,
                                                    Stroke::new(
                                                        2.0,
                                                        egui::Color32::from_rgb(247, 196, 97),
                                                    ),
                                                    egui::StrokeKind::Outside,
                                                );
                                            }

                                            ribbon_action = show_ribbon_image_controls(
                                                ui,
                                                image_rect,
                                                row_rect,
                                                index,
                                                pages_len,
                                            )
                                            .or(ribbon_action);

                                            if index + 1 < pages_len {
                                                page_boundary_screen_positions.push(image_rect.bottom());
                                            }
                                            manual_overlay = Some((image_rect, page.original_size, width_scale));
                                        }
                                        if let Some((image_rect, original_size, width_scale)) = manual_overlay
                                            && self.should_show_manual_cut_guides(index) {
                                                cut_marker_screen_positions.extend(self.draw_manual_cut_guides(
                                                    ui,
                                                    index,
                                                    image_rect,
                                                    original_size,
                                                    width_scale,
                                                ));
                                            }
                                    });
                                    if index + 1 < pages_len {
                                        ui.add_space(RIBBON_PREVIEW_SPACING);
                                    }
                                }
                                self.selected_ribbon_page =
                                    selected_page.map(|index| index.min(pages_len.saturating_sub(1)));
                                self.manual_cut_context_guide = manual_cut_context_guide;
                                self.apply_ribbon_action(ribbon_action);
                                if let Some(guide) = add_manual_cut_at {
                                    self.add_manual_cut_guide(guide);
                                }
                            });
                        self.draw_manual_cut_scroll_markers(
                            &scroll_output,
                            ui.painter(),
                            &cut_marker_screen_positions,
                            &page_boundary_screen_positions,
                        );
                    });
            },
        );
        draw_ribbon_resize_handles(ui, outer_rect, container_width);
    }

    /// Runs the ribbon's left/right edge drag handles and returns the resolved container width in
    /// points.
    ///
    /// The result is always within `RIBBON_MIN_WIDTH_FRACTION..=1.0` of `outer_rect.width()`.
    /// `self.ribbon_width_fraction` is updated as a fraction (so the ribbon keeps its relative
    /// width when the window is resized) and is written to user config only when a drag ends,
    /// never per frame. Painting of the handle bars is deliberately left to
    /// `draw_ribbon_resize_handles`, which runs after the ribbon frame so the bars are not
    /// covered by the frame fill.
    pub(super) fn apply_ribbon_resize(&mut self, ui: &mut Ui, outer_rect: egui::Rect) -> f32 {
        let available_width = safe_dimension(outer_rect.width(), 120.0);
        let min_width = available_width * RIBBON_MIN_WIDTH_FRACTION;
        let mut width =
            (clamp_ribbon_width_fraction(self.ribbon_width_fraction) * available_width).clamp(min_width, available_width);

        let left_x = outer_rect.center().x - width * 0.5;
        let right_x = outer_rect.center().x + width * 0.5;
        let left_handle = egui::Rect::from_min_max(
            egui::pos2(left_x - RIBBON_RESIZE_GRAB_WIDTH * 0.5, outer_rect.min.y),
            egui::pos2(left_x + RIBBON_RESIZE_GRAB_WIDTH * 0.5, outer_rect.max.y),
        );
        let right_handle = egui::Rect::from_min_max(
            egui::pos2(right_x - RIBBON_RESIZE_GRAB_WIDTH * 0.5, outer_rect.min.y),
            egui::pos2(right_x + RIBBON_RESIZE_GRAB_WIDTH * 0.5, outer_rect.max.y),
        );

        let left_response = ui.interact(
            left_handle,
            egui::Id::new("launcher_new_project_ribbon_resize_left"),
            egui::Sense::drag(),
        );
        let right_response = ui.interact(
            right_handle,
            egui::Id::new("launcher_new_project_ribbon_resize_right"),
            egui::Sense::drag(),
        );

        if left_response.hovered()
            || left_response.dragged()
            || right_response.hovered()
            || right_response.dragged()
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }

        // The container is centered, so each edge moves by half the width change: the delta is
        // doubled to keep the grabbed edge under the pointer. A left-edge drag mirrors the sign.
        if left_response.dragged() {
            width = (width - left_response.drag_delta().x * 2.0).clamp(min_width, available_width);
        }
        if right_response.dragged() {
            width = (width + right_response.drag_delta().x * 2.0).clamp(min_width, available_width);
        }

        self.ribbon_width_fraction = clamp_ribbon_width_fraction(width / available_width);
        if left_response.drag_stopped() || right_response.drag_stopped() {
            save_ribbon_width_fraction(self.ribbon_width_fraction);
        }
        width
    }

    pub(super) fn can_start_stitch(&self) -> bool {
        !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    pub(super) fn apply_source_import_result(&mut self, source_path: PathBuf, pages: Vec<RibbonPage>) {
        self.simple_stitch_done = false;
        self.simple_manual_cut_preview_active = false;
        self.advance_simple_import_step_after_success();
        if self.import_mode == ImportMode::ReplaceRibbon || self.ribbon.pages().is_empty() {
            self.ribbon.replace_source(source_path, pages);
            self.selected_ribbon_page = self.default_selected_page();
            return;
        }

        let insert_at = match self.import_mode {
            ImportMode::ReplaceRibbon => 0,
            ImportMode::AddToStart => 0,
            ImportMode::AddToEnd => self.ribbon.pages().len(),
            ImportMode::AddBeforeCurrent => self
                .selected_ribbon_page
                .unwrap_or(0)
                .min(self.ribbon.pages().len()),
            ImportMode::AddAfterCurrent => self
                .selected_ribbon_page
                .map_or(self.ribbon.pages().len(), |index| {
                    index.saturating_add(1).min(self.ribbon.pages().len())
                }),
        };
        let inserted_range = self.ribbon.insert_pages(source_path, insert_at, pages);
        self.selected_ribbon_page = Some(inserted_range.start);
    }

    pub(super) fn default_selected_page(&self) -> Option<usize> {
        (!self.ribbon.pages().is_empty()).then_some(0)
    }

    pub(super) fn selection_after_removal(&self, removed_index: usize) -> Option<usize> {
        let remaining_pages = self.ribbon.pages().len();
        if remaining_pages == 0 {
            None
        } else if removed_index >= remaining_pages {
            Some(remaining_pages - 1)
        } else {
            Some(removed_index)
        }
    }

    pub(super) fn swap_manual_cut_guide_pages(&mut self, first_index: usize, second_index: usize) {
        for guide in &mut self.manual_cut_guides {
            if guide.page_index == first_index {
                guide.page_index = second_index;
            } else if guide.page_index == second_index {
                guide.page_index = first_index;
            }
        }
    }

    pub(super) fn remove_manual_cut_guide_page(&mut self, removed_index: usize) {
        self.manual_cut_guides
            .retain(|guide| guide.page_index != removed_index);
        for guide in &mut self.manual_cut_guides {
            if guide.page_index > removed_index {
                guide.page_index -= 1;
            }
        }
    }

    pub(super) fn merge_manual_cut_guide_pages(&mut self, first_index: usize, first_height: usize) {
        for guide in &mut self.manual_cut_guides {
            if guide.page_index == first_index + 1 {
                guide.page_index = first_index;
                guide.y = guide.y.saturating_add(first_height);
            } else if guide.page_index > first_index + 1 {
                guide.page_index -= 1;
            }
        }
    }

    pub(super) fn merge_ribbon_pages(&mut self, first_index: usize) {
        let Some(first_height) = self
            .ribbon
            .pages()
            .get(first_index)
            .map(|page| page.original_size[1])
        else {
            return;
        };
        match self.ribbon.merge_with_next(first_index) {
            Ok(()) => {
                self.merge_manual_cut_guide_pages(first_index, first_height);
                self.clamp_manual_cut_guides_to_current_pages();
                self.selected_ribbon_page = Some(first_index);
                self.import_status = t!("launcher.new_project.pages_stitched_status").to_string();
                self.last_error = None;
            }
            Err(RibbonMergeError::WidthMismatch {
                first_name,
                first_width,
                second_name,
                second_width,
            }) => {
                self.last_error = Some(tf!("launcher.new_project.stitch_width_mismatch_error", first_name = first_name, first_width = first_width, second_name = second_name, second_width = second_width));
                self.import_status = t!("launcher.new_project.stitch_pages_cancelled").to_string();
            }
            Err(RibbonMergeError::MissingPage) => {
                self.last_error = Some(t!("launcher.new_project.no_adjacent_page_error").to_string());
                self.import_status = t!("launcher.new_project.stitch_pages_unavailable").to_string();
            }
        }
    }

    pub(super) fn apply_ribbon_action(&mut self, action: Option<(usize, RibbonImageControlAction)>) {
        let Some((index, action)) = action else {
            return;
        };
        match action {
            RibbonImageControlAction::Crop => self.open_crop_editor(index),
            RibbonImageControlAction::MoveUp => {
                if self.ribbon.move_page_up(index) {
                    self.swap_manual_cut_guide_pages(index - 1, index);
                    self.selected_ribbon_page = Some(index - 1);
                    self.import_status = t!("launcher.new_project.image_moved_up").to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::MoveDown => {
                if self.ribbon.move_page_down(index) {
                    self.swap_manual_cut_guide_pages(index, index + 1);
                    self.selected_ribbon_page = Some(index + 1);
                    self.import_status = t!("launcher.new_project.image_moved_down").to_string();
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::Delete => {
                if let Some(removed_page) = self.ribbon.remove_page(index) {
                    self.selected_ribbon_page = self.selection_after_removal(index);
                    self.remove_manual_cut_guide_page(index);
                    self.clamp_manual_cut_guides_to_current_pages();
                    self.import_status =
                        tf!("launcher.new_project.image_removed_status", removed_page = removed_page.name);
                    self.last_error = None;
                }
            }
            RibbonImageControlAction::MergeWithPrevious => {
                if index > 0 {
                    self.merge_ribbon_pages(index - 1);
                }
            }
            RibbonImageControlAction::MergeWithNext => {
                self.merge_ribbon_pages(index);
            }
        }
    }

    pub(super) fn has_manual_cut_preview(&self) -> bool {
        !self.manual_cut_guides.is_empty()
    }

    pub(super) fn should_show_manual_cut_guides(&self, page_index: usize) -> bool {
        self.manual_cut_guides
            .iter()
            .any(|guide| guide.page_index == page_index)
    }

    pub(super) fn add_manual_cut_guide(&mut self, guide: ManualCutGuide) {
        let Some(page) = self.ribbon.pages().get(guide.page_index) else {
            return;
        };
        if !manual_cut_y_is_valid(guide.y, page.original_size[1]) {
            self.last_error = Some(tf!("launcher.new_project.cut_line_min_distance_error", manual_cut_min_edge_distance_px = MANUAL_CUT_MIN_EDGE_DISTANCE_PX));
            self.import_status = t!("launcher.new_project.cut_line_not_added").to_string();
            return;
        };
        self.manual_cut_guides.push(guide);
        self.clamp_manual_cut_guides_to_current_pages();
        self.import_status = tf!("launcher.new_project.manual_cut_added_status", arg = self.manual_cut_guides.len());
        self.last_error = None;
    }

    pub(super) fn start_stitch_split(&mut self, mode: StitchSplitMode) {
        let Some(options) = self.parse_stitch_options(mode) else {
            return;
        };
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.stitch_no_images_error").to_string();
            return;
        }
        self.manual_cut_guides.clear();
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: stitch_mode_initial_stage(mode).to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = stitch_mode_start_status(mode).to_string();
        self.stitch
            .begin(StitchRequest::StitchSplit { images, options });
    }

    pub(super) fn restore_original_pages(&mut self) {
        if self.ribbon.restore_original() {
            self.selected_ribbon_page = self.default_selected_page();
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.source_images_restored").to_string();
            self.last_error = None;
        } else {
            self.last_error = Some(t!("launcher.new_project.source_images_not_loaded").to_string());
        }
    }

    pub(super) fn apply_manual_cut_guides(&mut self) {
        if !self.has_manual_cut_preview() || self.stitch.is_loading() {
            return;
        }
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.manual_cut_needs_page_error").to_string());
            return;
        }
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: "split".to_string(),
            current: 0,
            total: self.manual_cut_guides.len().saturating_add(1).max(1),
        });
        self.import_status = t!("launcher.new_project.manual_cut_progress").to_string();
        self.stitch.begin(StitchRequest::ApplyManualCutsToPages {
            images,
            cut_guides: self.manual_cut_guides.clone(),
        });
    }

    pub(super) fn start_cut_like_project_chapter(&mut self) {
        let Some(title) = self.current_cut_title_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_title_for_example_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
            return;
        };
        let Some(chapter) = self.current_cut_chapter_name().map(str::to_string) else {
            self.last_error = Some(t!("launcher.new_project.select_chapter_for_example_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
            return;
        };

        let src_dir = self
            .project_catalog
            .projects_root()
            .join(&title)
            .join(&chapter)
            .join(config::SRC_DIR);
        let reference_dir = if src_dir.exists() {
            src_dir
        } else {
            let legacy_dir = self
                .project_catalog
                .projects_root()
                .join(&title)
                .join(&chapter)
                .join("scr");
            if legacy_dir.exists() {
                legacy_dir
            } else {
                self.last_error = Some(t!("launcher.new_project.example_no_src_folder_error").to_string());
                self.import_status = t!("launcher.new_project.cut_by_example_unavailable").to_string();
                return;
            }
        };

        self.start_cut_like_reference(
            reference_dir,
            &tf!("launcher.new_project.cut_by_example_progress", title = title, chapter = chapter),
        );
    }

    /// Opens the reference-folder picker without blocking; `poll_cut_like_folder_dialog`
    /// starts the cut once a folder is chosen.
    pub(super) fn start_cut_like_folder(&mut self, ctx: &egui::Context) {
        if self.cut_like_folder_dialog.is_some() {
            return;
        }
        let request = FileDialogRequest::new(DialogKind::OpenFolder);
        self.cut_like_folder_dialog = Some(file_dialog::request(request, Some(repaint_waker(ctx))));
    }

    /// Applies the reference-folder picker's answer: a folder starts the cut, a cancel changes
    /// nothing, a dialog failure is reported in the error line.
    pub(super) fn poll_cut_like_folder_dialog(&mut self) {
        let Some(pending) = self.cut_like_folder_dialog.as_mut() else {
            return;
        };
        let Some(result) = pending.poll() else {
            return;
        };
        self.cut_like_folder_dialog = None;
        let folder = match result {
            Ok(Some(picked)) => picked.into_folder(),
            Ok(None) => return,
            Err(err) => {
                self.report_dialog_error("cut-like-reference folder", &err);
                return;
            }
        };
        let Some(folder) = folder else {
            // `OpenFolder` always answers with `Picked::Folder` (ms-platform contract).
            self.report_dialog_error("cut-like-reference folder", &FileDialogError::WorkerLost);
            return;
        };
        self.start_cut_like_reference(
            folder.clone(),
            &tf!("launcher.new_project.cut_by_folder_progress", folder = folder.display()),
        );
    }

    /// Logs a failed file dialog with its typed error and shows its localized message.
    pub(super) fn report_dialog_error(&mut self, what: &str, err: &FileDialogError) {
        ms_log::runtime_log::log_warn(format!("[new-project] {what} dialog failed: {err:?}"));
        self.last_error = Some(err.user_message());
    }

    pub(super) fn start_cut_like_reference(&mut self, reference_dir: PathBuf, status_message: &str) {
        let images = self.current_stitch_images();
        if images.is_empty() {
            self.last_error = Some(t!("launcher.new_project.open_or_download_chapter_error").to_string());
            self.import_status = t!("launcher.new_project.cut_by_example_empty_ribbon_error").to_string();
            return;
        }

        self.manual_cut_guides.clear();
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "stitch",
            stage: "decode".to_string(),
            current: 0,
            total: 1,
        });
        self.import_status = status_message.to_string();
        ms_log::runtime_log::log_info(format!(
            "[new-project] starting reference-based cut using '{}'",
            reference_dir.display(),
        ));
        self.stitch.begin(StitchRequest::CutLikeReference {
            images,
            reference_dir,
        });
    }

    pub(super) fn clamp_manual_cut_guides_to_current_pages(&mut self) {
        let pages = self.ribbon.pages();
        self.manual_cut_guides.retain(|guide| {
            pages
                .get(guide.page_index)
                .is_some_and(|page| manual_cut_y_is_valid(guide.y, page.original_size[1]))
        });
        self.manual_cut_guides
            .sort_unstable_by_key(|guide| (guide.page_index, guide.y));
        self.manual_cut_guides
            .dedup_by_key(|guide| (guide.page_index, guide.y));
    }

    pub(super) fn manual_cut_drag_bounds(
        &self,
        guide_index: usize,
        page_index: usize,
        page_height: usize,
    ) -> (usize, usize) {
        let min_y = MANUAL_CUT_MIN_EDGE_DISTANCE_PX;
        let max_y = page_height.saturating_sub(MANUAL_CUT_MIN_EDGE_DISTANCE_PX);
        let previous_y = self
            .manual_cut_guides
            .iter()
            .enumerate()
            .filter(|(index, guide)| *index != guide_index && guide.page_index == page_index)
            .filter_map(|(_, guide)| {
                (guide.y < self.manual_cut_guides[guide_index].y).then_some(guide.y)
            })
            .max();
        let next_y = self
            .manual_cut_guides
            .iter()
            .enumerate()
            .filter(|(index, guide)| *index != guide_index && guide.page_index == page_index)
            .filter_map(|(_, guide)| {
                (guide.y > self.manual_cut_guides[guide_index].y).then_some(guide.y)
            })
            .min();
        let lower_bound = previous_y.map_or(min_y, |y| y.saturating_add(1).max(min_y));
        let upper_bound = next_y.map_or(max_y, |y| y.saturating_sub(1).min(max_y));
        (lower_bound, upper_bound.max(lower_bound))
    }

    pub(super) fn draw_manual_cut_guides(
        &mut self,
        ui: &mut Ui,
        page_index: usize,
        image_rect: egui::Rect,
        original_size: [usize; 2],
        width_scale: f32,
    ) -> Vec<f32> {
        let painter = ui.painter();
        let image_height = original_size[1].max(1);
        let mut screen_positions = Vec::with_capacity(self.manual_cut_guides.len());
        let mut index = 0;
        while index < self.manual_cut_guides.len() {
            if self.manual_cut_guides[index].page_index != page_index {
                index += 1;
                continue;
            }
            let current = self.manual_cut_guides[index].y;
            let interaction_y = image_rect.top() + current as f32 * width_scale;
            let interaction_rect = egui::Rect::from_center_size(
                egui::pos2(image_rect.center().x, interaction_y),
                egui::vec2(MANUAL_CUT_HANDLE_WIDTH, MANUAL_CUT_HANDLE_HEIGHT),
            );
            let response = ui.interact(
                interaction_rect,
                ui.id().with(("manual_cut_handle", index)),
                egui::Sense::drag(),
            );
            if response.dragged() {
                let pointer_y = ui
                    .ctx()
                    .pointer_interact_pos()
                    .map(|position| position.y)
                    .unwrap_or(interaction_y);
                let image_y = ((pointer_y - image_rect.top()) / width_scale)
                    .round()
                    .clamp(
                        MANUAL_CUT_MIN_EDGE_DISTANCE_PX as f32,
                        image_height.saturating_sub(MANUAL_CUT_MIN_EDGE_DISTANCE_PX) as f32,
                    ) as usize;
                let (lower_bound, upper_bound) =
                    self.manual_cut_drag_bounds(index, page_index, image_height);
                self.manual_cut_guides[index].y = image_y.clamp(lower_bound, upper_bound);
            }

            let y = image_rect.top() + self.manual_cut_guides[index].y as f32 * width_scale;
            screen_positions.push(y);
            let line_start = egui::pos2(image_rect.left(), y);
            let line_end = egui::pos2(image_rect.right(), y);
            painter.line_segment(
                [line_start, line_end],
                Stroke::new(2.0, egui::Color32::from_rgb(255, 59, 48)),
            );

            let handle_rect = egui::Rect::from_center_size(
                egui::pos2(image_rect.center().x, y),
                egui::vec2(MANUAL_CUT_HANDLE_WIDTH, MANUAL_CUT_HANDLE_HEIGHT),
            );
            painter.rect_filled(handle_rect, 8.0, egui::Color32::from_rgb(190, 28, 28));
            painter.text(
                handle_rect.center(),
                egui::Align2::CENTER_CENTER,
                "^  v",
                egui::FontId::proportional(13.0),
                egui::Color32::WHITE,
            );

            let delete_center = egui::pos2(
                handle_rect.right() + MANUAL_CUT_DELETE_BUTTON_SIZE * 0.35,
                handle_rect.top() - MANUAL_CUT_DELETE_BUTTON_SIZE * 0.25,
            );
            let delete_rect = egui::Rect::from_center_size(
                delete_center,
                egui::vec2(MANUAL_CUT_DELETE_BUTTON_SIZE, MANUAL_CUT_DELETE_BUTTON_SIZE),
            );
            let delete_response = ui.interact(
                delete_rect,
                ui.id().with(("manual_cut_delete", index)),
                egui::Sense::click(),
            );
            painter.circle_filled(
                delete_rect.center(),
                MANUAL_CUT_DELETE_BUTTON_SIZE * 0.5,
                egui::Color32::from_rgb(220, 0, 0),
            );
            painter.circle_stroke(
                delete_rect.center(),
                MANUAL_CUT_DELETE_BUTTON_SIZE * 0.5,
                Stroke::new(1.0, egui::Color32::from_rgb(255, 120, 120)),
            );
            painter.text(
                delete_rect.center(),
                egui::Align2::CENTER_CENTER,
                "x",
                egui::FontId::proportional(15.0),
                egui::Color32::WHITE,
            );
            if delete_response.clicked() {
                self.manual_cut_guides.remove(index);
                screen_positions.pop();
                self.import_status = tf!("launcher.new_project.cut_line_removed_status", arg = self.manual_cut_guides.len());
                continue;
            }
            index += 1;
        }
        screen_positions
    }

    /// Draws cut and page-boundary arrows in the markable scrollbar gutter.
    ///
    /// `cut`/`page_boundary` positions are screen Y values collected while the
    /// ribbon content is rendered; they are converted back to content space and
    /// projected onto the bar via the widget gutter. Page boundaries (blue) are
    /// drawn under cut markers (red).
    pub(super) fn draw_manual_cut_scroll_markers(
        &self,
        output: &MarkedScrollOutput<()>,
        painter: &egui::Painter,
        cut_screen_positions: &[f32],
        page_boundary_screen_positions: &[f32],
    ) {
        if output.content_size.y <= 1.0 {
            return;
        }
        // Screen Y collected during rendering -> content-space Y for the gutter.
        let to_content = |screen_y: f32| screen_y - output.inner_rect.top() + output.offset.y;

        let page_boundary_style = ArrowStyle {
            width: PAGE_BOUNDARY_SCROLL_ARROW_WIDTH,
            height: PAGE_BOUNDARY_SCROLL_ARROW_HEIGHT,
            fill: egui::Color32::from_rgb(40, 132, 255),
            stroke: Stroke::new(1.0, egui::Color32::from_rgb(130, 190, 255)),
            tail_length: 10.0 / 3.0,
            tail_width: 4.0 / 3.0,
        };
        let manual_cut_style = ArrowStyle {
            width: MANUAL_CUT_SCROLL_ARROW_WIDTH,
            height: MANUAL_CUT_SCROLL_ARROW_HEIGHT,
            fill: egui::Color32::from_rgb(255, 0, 0),
            stroke: Stroke::new(2.0, egui::Color32::from_rgb(255, 86, 86)),
            tail_length: 10.0,
            tail_width: 4.0,
        };

        let mut items: Vec<GutterItem> = Vec::new();
        items.extend(page_boundary_screen_positions.iter().map(|&screen_y| {
            arrow(
                ScrollSpan::pixel_at(to_content(screen_y)),
                page_boundary_style,
            )
            .layer(0)
        }));
        items.extend(cut_screen_positions.iter().map(|&screen_y| {
            arrow(ScrollSpan::pixel_at(to_content(screen_y)), manual_cut_style).layer(1)
        }));
        output.paint_gutter(painter, items);
    }
}

/// Pixel width that one ribbon page's on-screen scale is measured against.
///
/// In uniform mode the base is the page's own width, so every page fills the ribbon content width
/// exactly. In proportional mode all pages share the widest page's width, so their on-screen
/// widths keep the ratio of their source widths. Never returns 0, so the caller's division is
/// always safe; a `widest_page_px` smaller than `page_width_px` (impossible for a real ribbon) is
/// raised to the page width rather than upscaling the page past the ribbon.
pub(super) fn ribbon_scale_base_px(uniform: bool, page_width_px: usize, widest_page_px: usize) -> usize {
    if uniform {
        page_width_px.max(1)
    } else {
        widest_page_px.max(page_width_px).max(1)
    }
}

/// Uniform scale factor and on-screen size (points) for one ribbon page.
///
/// `content_width` is the ribbon's usable width in points, `original_size` the page's post-crop
/// pixel size `[w, h]`, and `scale_base_px` the pixel width that maps onto the full content width
/// (see `ribbon_scale_base_px`). The same factor is applied to both axes, so the aspect ratio is
/// preserved. A non-finite or degenerate `content_width` falls back to 120pt via `safe_dimension`.
pub(super) fn ribbon_page_display_size(
    content_width: f32,
    original_size: [usize; 2],
    scale_base_px: usize,
) -> (f32, egui::Vec2) {
    let content_width = safe_dimension(content_width, 120.0);
    let scale = content_width / scale_base_px.max(1) as f32;
    (
        scale,
        egui::vec2(
            original_size[0] as f32 * scale,
            original_size[1] as f32 * scale,
        ),
    )
}

/// Left inset (points) that centers a page of `image_width` inside a `content_width` ribbon row.
///
/// Returns `0.0` when the page already fills the row (uniform mode) or when either value is
/// non-finite, so the page can never be pushed off the left edge of the ribbon.
pub(super) fn ribbon_page_left_inset(content_width: f32, image_width: f32) -> f32 {
    let inset = (content_width - image_width) * 0.5;
    if inset.is_finite() { inset.max(0.0) } else { 0.0 }
}

/// Clamps a ribbon width fraction into `RIBBON_MIN_WIDTH_FRACTION..=1.0`.
///
/// A non-finite value (a corrupt config entry, or a division by a degenerate width) falls back to
/// `1.0` so the ribbon can never end up hidden or inverted.
pub(super) fn clamp_ribbon_width_fraction(fraction: f32) -> f32 {
    if fraction.is_finite() {
        fraction.clamp(RIBBON_MIN_WIDTH_FRACTION, 1.0)
    } else {
        1.0
    }
}

/// Paints the two ribbon resize handle bars on the container edges.
///
/// Called after the ribbon frame so the frame fill cannot cover the inner half of each bar.
/// `container_width` is the resolved width returned by `apply_ribbon_resize`.
pub(super) fn draw_ribbon_resize_handles(ui: &Ui, outer_rect: egui::Rect, container_width: f32) {
    let painter = ui.painter();
    let half = container_width * 0.5;
    for x in [outer_rect.center().x - half, outer_rect.center().x + half] {
        let handle_rect = egui::Rect::from_center_size(
            egui::pos2(x, outer_rect.center().y),
            egui::vec2(
                RIBBON_RESIZE_HANDLE_WIDTH,
                (outer_rect.height() - 12.0).max(48.0),
            ),
        );
        painter.rect_filled(handle_rect, 4.0, egui::Color32::from_rgb(64, 64, 70));
    }
}

pub(super) fn manual_cut_y_is_valid(y: usize, page_height: usize) -> bool {
    y >= MANUAL_CUT_MIN_EDGE_DISTANCE_PX
        && y.saturating_add(MANUAL_CUT_MIN_EDGE_DISTANCE_PX) <= page_height
}

pub(super) fn ribbon_control_button(label: &str, fill: egui::Color32, enabled: bool) -> Button<'static> {
    let (fill, stroke, text_color) = if enabled {
        (
            fill,
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 92),
            egui::Color32::WHITE,
        )
    } else {
        (
            egui::Color32::from_rgba_premultiplied(72, 76, 84, 220),
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 28),
            egui::Color32::from_rgba_premultiplied(255, 255, 255, 80),
        )
    };
    Button::new(RichText::new(label).size(13.0).strong().color(text_color))
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(999.0)
}

/// Draws the floating crop/move/delete button group for one ribbon page.
///
/// `image_rect` is the page image on screen and `bounds` the full-width ribbon content row it sits
/// in. The group is anchored to the image's right edge, but is kept inside `bounds` so a page
/// narrower than the ~148pt group (proportional mode on a shrunken ribbon) cannot push the buttons
/// out of the visible ribbon. For a page wider than the group the clamp is inert.
pub(super) fn show_ribbon_image_controls(
    ui: &mut Ui,
    image_rect: egui::Rect,
    bounds: egui::Rect,
    index: usize,
    pages_len: usize,
) -> Option<(usize, RibbonImageControlAction)> {
    let can_move_up = index > 0;
    let can_move_down = index + 1 < pages_len;
    let button_span = RIBBON_DELETE_BUTTON_SIZE * 3.0 + RIBBON_IMAGE_CONTROL_GAP * 2.0;
    let group_width =
        RIBBON_CROP_BUTTON_WIDTH + RIBBON_IMAGE_CONTROL_GAP + RIBBON_DELETE_BUTTON_SIZE;
    let sticky_top = (ui.clip_rect().top() + 8.0).clamp(
        image_rect.top() + 8.0,
        (image_rect.bottom() - button_span - 8.0).max(image_rect.top() + 8.0),
    );
    let preferred_left = (image_rect.right() - group_width - 8.0).max(image_rect.left() + 8.0);
    let min_left = bounds.left() + 8.0;
    let max_left = bounds.right() - group_width - 8.0;
    let sticky_left = if max_left >= min_left {
        preferred_left.clamp(min_left, max_left)
    } else {
        // The ribbon itself is narrower than the button group: left-align and let it overflow
        // rather than invert the clamp.
        min_left
    };
    let controls_rect = egui::Rect::from_min_size(
        egui::pos2(sticky_left, sticky_top),
        egui::vec2(group_width, button_span),
    );
    let crop_rect = egui::Rect::from_min_size(
        controls_rect.min,
        egui::vec2(RIBBON_CROP_BUTTON_WIDTH, RIBBON_DELETE_BUTTON_SIZE),
    );
    let up_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            controls_rect.top(),
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );
    let down_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            up_rect.bottom() + RIBBON_IMAGE_CONTROL_GAP,
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );
    let delete_rect = egui::Rect::from_min_size(
        egui::pos2(
            controls_rect.right() - RIBBON_DELETE_BUTTON_SIZE,
            down_rect.bottom() + RIBBON_IMAGE_CONTROL_GAP,
        ),
        egui::vec2(RIBBON_DELETE_BUTTON_SIZE, RIBBON_DELETE_BUTTON_SIZE),
    );

    if ui.put(crop_rect, ribbon_crop_button()).clicked() {
        return Some((index, RibbonImageControlAction::Crop));
    }
    if ui
        .put(
            up_rect,
            ribbon_control_button("/\\", egui::Color32::from_rgb(52, 104, 173), can_move_up),
        )
        .clicked()
        && can_move_up
    {
        return Some((index, RibbonImageControlAction::MoveUp));
    }
    if ui
        .put(
            down_rect,
            ribbon_control_button("\\/", egui::Color32::from_rgb(52, 104, 173), can_move_down),
        )
        .clicked()
        && can_move_down
    {
        return Some((index, RibbonImageControlAction::MoveDown));
    }
    if ui
        .put(
            delete_rect,
            ribbon_control_button("×", egui::Color32::from_rgb(190, 48, 48), true),
        )
        .clicked()
    {
        return Some((index, RibbonImageControlAction::Delete));
    }
    None
}

pub(super) fn ribbon_crop_button() -> Button<'static> {
    Button::new(
        RichText::new(t!("launcher.new_project.crop_menu"))
            .size(13.0)
            .strong()
            .color(egui::Color32::WHITE),
    )
    .fill(egui::Color32::from_rgb(174, 103, 24))
    .stroke(Stroke::new(
        1.0,
        egui::Color32::from_rgba_premultiplied(255, 220, 170, 150),
    ))
    .corner_radius(10.0)
}
