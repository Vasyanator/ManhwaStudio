/*
File: crates/ms-launcher/src/new_project/window/advanced_ui.rs

Purpose:
Advanced (browser-driven) downloader UI of the New Project window and its auto-review window:
the form, the start/finish wrappers around `AdvancedDownloadController` commands, the
candidate-card review layout and the URL-prefix presets.

Key items:
- `show_advanced_downloader`, `show_advanced_auto_review_window`, `apply_advanced_auto_review`.
- `start_advanced_*` / `finish_advanced_*`, `prepare_simple_deep_capture`, `save_advanced_prefix`.
- `auto_review_card_layout` and the auto-review default-selection helpers.

Notes:
Browser work runs in the AI backend over IPC (`advanced_download.rs`); this file only issues
commands and draws polled results.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn show_advanced_downloader(&mut self, ui: &mut Ui) {
        field_label(ui, t!("launcher.new_project.page_link_label"));
        ui.add(
            TextEdit::singleline(&mut self.advanced_page_url)
                .desired_width(fill_width(ui))
                .hint_text(t!("launcher.new_project.open_page_in_browser_hint")),
        );

        field_label(ui, t!("launcher.new_project.browser_engine_label"));
        ui.add_enabled_ui(
            !self.advanced_download.is_loading()
                && !self.advanced_link_collect_active
                && !self.advanced_intercept_active,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for backend in AdvancedBrowserBackend::ALL {
                        ui.selectable_value(
                            &mut self.selected_advanced_backend,
                            backend,
                            backend.label(),
                        );
                    }
                });
            },
        );
        self.advanced_download
            .set_backend(self.selected_advanced_backend);

        field_label(ui, t!("launcher.new_project.browser_label"));
        self.clamp_advanced_indexes();
        if self.selected_advanced_backend == AdvancedBrowserBackend::Cloak {
            ui.label(RichText::new("CloakBrowser").small());
            ui.label(
                RichText::new(t!("launcher.new_project.cloak_profile_hint"))
                    .small()
                    .weak(),
            );
        } else if self.browser_names.is_empty() {
            ui.label(
                RichText::new(t!("launcher.new_project.no_supported_browsers_hint"))
                    .small()
                    .weak(),
            );
        } else {
            combo_index_owned(
                ui,
                "launcher_new_project_browser",
                &self.browser_names,
                &mut self.selected_browser,
            );
        }

        let can_open_browser = !self.advanced_download.is_loading()
            && !self.advanced_link_collect_active
            && !self.advanced_intercept_active
            && self.advanced_browser_available()
            && !self.advanced_page_url.trim().is_empty();
        if button_sized(
            ui,
            t!("launcher.new_project.open_in_browser_button"),
            egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
            can_open_browser,
        )
        .clicked()
        {
            self.start_advanced_open();
        }
        ui.label(
            RichText::new(t!("launcher.new_project.ensure_images_loaded_hint"))
                .small()
                .weak(),
        );

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        if self.selected_advanced_backend != AdvancedBrowserBackend::Cloak
            && self.advanced_mode == AdvancedDownloadMode::DeepCapture
        {
            self.advanced_mode = AdvancedDownloadMode::PatternLinkSearch;
        }

        field_label(ui, t!("launcher.new_project.mode_label"));
        ui.add_enabled_ui(
            !self.advanced_link_collect_active && !self.advanced_intercept_active,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::PatternLinkSearch,
                        t!("launcher.new_project.mode_pattern_search"),
                    );
                    ui.selectable_value(
                        &mut self.advanced_mode,
                        AdvancedDownloadMode::CanvasDownload,
                        t!("launcher.new_project.mode_canvas_download"),
                    );
                    ui.add_enabled_ui(
                        self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
                        |ui| {
                            ui.selectable_value(
                                &mut self.advanced_mode,
                                AdvancedDownloadMode::DeepCapture,
                                t!("launcher.new_project.mode_deep_intercept"),
                            );
                        },
                    );
                });
            },
        );
        ui.add_space(8.0);

        if self.advanced_mode == AdvancedDownloadMode::PatternLinkSearch {
            ui.add_enabled_ui(
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
                |ui| {
                    field_label(ui, t!("launcher.new_project.link_search_type_label"));
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(
                            &mut self.advanced_link_source_mode,
                            AdvancedLinkSourceMode::Pattern,
                            t!("launcher.new_project.link_search_normal"),
                        );
                        ui.selectable_value(
                            &mut self.advanced_link_source_mode,
                            AdvancedLinkSourceMode::AutoReview,
                            t!("launcher.new_project.link_search_autofind"),
                        );
                    });
                },
            );
            ui.add_space(6.0);

            if self.advanced_link_source_mode == AdvancedLinkSourceMode::Pattern {
                field_label(
                    ui,
                    t!("launcher.new_project.site_preset_prefix_hint"),
                );
                let previous_site = self.selected_site;
                combo_index_pairs(
                    ui,
                    "launcher_new_project_site",
                    &self.site_presets,
                    &mut self.selected_site,
                );
                if previous_site != self.selected_site
                    && let Some((_, prefix)) = self.site_presets.get(self.selected_site)
                {
                    self.image_prefix = prefix.clone();
                }

                field_label(ui, t!("launcher.new_project.prefix_label"));
                ui.add(TextEdit::singleline(&mut self.image_prefix).desired_width(fill_width(ui)));

                field_label(ui, t!("launcher.new_project.new_site_name_label"));
                ui.add(
                    TextEdit::singleline(&mut self.site_name)
                        .desired_width(fill_width(ui))
                        .hint_text(t!("launcher.new_project.save_name_placeholder")),
                );

                if button_sized(
                    ui,
                    t!("launcher.new_project.save_prefix_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active,
                )
                .clicked()
                {
                    self.save_advanced_prefix();
                }
            } else {
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.autofind_hint"),
                    )
                    .small()
                    .weak(),
                );
            }

            field_label(ui, t!("launcher.new_project.download_threads_label"));
            ui.add_enabled(
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active,
                WheelSlider::new(&mut self.advanced_fetch_parallelism, 1..=8).text(t!("launcher.new_project.threads_suffix")),
            );

            sub_group(ui, t!("launcher.new_project.collect_and_download_button"), |ui| {
                if self.advanced_link_collect_active {
                    ui.label(
                        RichText::new(tf!("launcher.new_project.collected_links_status", arg = self.advanced_link_collect_found_links))
                        .color(egui::Color32::from_rgb(76, 175, 80))
                        .strong(),
                    );
                    ui.add_space(8.0);
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.download_now_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.start_advanced_fetch();
                }
                if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview
                    && button_sized(
                        ui,
                        t!("launcher.new_project.stop_download_button"),
                        egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                        self.advanced_download.can_cancel_current_auto_fetch(),
                    )
                    .clicked()
                {
                    self.stop_advanced_auto_fetch();
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.start_collect_links_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && !self.advanced_link_collect_active
                        && !self.advanced_intercept_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.start_advanced_link_collect();
                }
                if button_sized(
                    ui,
                    t!("launcher.new_project.stop_collect_links_button"),
                    egui::vec2(LEFT_PANEL_WIDTH - 74.0, 34.0),
                    !self.advanced_download.is_loading()
                        && self.advanced_link_collect_active
                        && self.advanced_browser_available(),
                )
                .clicked()
                {
                    self.finish_advanced_link_collect();
                }
            });
        } else if self.advanced_mode == AdvancedDownloadMode::CanvasDownload {
            if self.advanced_intercept_active {
                ui.label(
                    RichText::new(tf!("launcher.new_project.found_pages_status", arg = self.advanced_intercept_counts.total))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            if button_sized(
                ui,
                t!("launcher.new_project.download_now_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.start_advanced_canvas_fetch();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.start_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.start_advanced_canvas_intercept();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.finish_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && self.advanced_intercept_active
                    && self.advanced_browser_available(),
            )
            .clicked()
            {
                self.finish_advanced_canvas_intercept();
            }
        } else {
            if self.advanced_intercept_active {
                let counts = self.advanced_intercept_counts;
                ui.label(
                    RichText::new(tf!("launcher.new_project.intercept_counts_status", counts = counts.canvases, counts_2 = counts.images))
                    .color(egui::Color32::from_rgb(76, 175, 80))
                    .strong(),
                );
                ui.add_space(8.0);
            }
            ui.label(
                RichText::new(
                    t!("launcher.new_project.cloak_deep_intercept_hint"),
                )
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            if button_sized(
                ui,
                t!("launcher.new_project.start_deep_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && !self.advanced_intercept_active
                    && self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
            )
            .clicked()
            {
                self.start_advanced_deep_intercept();
            }
            if button_sized(
                ui,
                t!("launcher.new_project.finish_deep_intercept_button"),
                egui::vec2(LEFT_PANEL_WIDTH - 52.0, 34.0),
                !self.advanced_download.is_loading()
                    && !self.advanced_link_collect_active
                    && self.advanced_intercept_active
                    && self.selected_advanced_backend == AdvancedBrowserBackend::Cloak,
            )
            .clicked()
            {
                self.finish_advanced_deep_intercept();
            }
        }
        self.show_operation_progress(ui, "advanced_download");
    }

    pub(super) fn show_advanced_auto_review_window(&mut self, ctx: &egui::Context) {
        let mut apply_clicked = false;
        let mut close_review = false;
        if let Some(review) = self.advanced_auto_review.as_mut() {
            let mut open = review.open;
            Window::new(t!("launcher.new_project.autofind_review_title")).id(egui::Id::new("launcher.new_project.autofind_review_title"))
                .open(&mut open)
                .resizable(true)
                .default_width(980.0)
                .default_height(720.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(&mut review.group_view, t!("launcher.new_project.split_by_groups_check"));
                        ui.separator();
                        ui.label(tf!("launcher.new_project.review_images_count", review = review.retained_count(), review_2 = review.candidates.items.len()));
                        ui.label(tf!("launcher.new_project.review_groups_count", review = review.candidates.groups.len()));
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(t!("launcher.new_project.add_to_ribbon_button")).clicked() {
                            apply_clicked = true;
                        }
                        if ui.button(t!("launcher.common.close_button")).clicked() {
                            close_review = true;
                        }
                    });
                    ui.separator();
                    ScrollArea::vertical()
                        .id_salt("advanced_auto_review_scroll")
                        .max_height(ui.available_height().max(160.0))
                        .show(ui, |ui| {
                            if review.group_view {
                                Self::show_auto_review_groups(ui, review);
                                ui.separator();
                                ui.heading(t!("launcher.new_project.final_order_label"));
                            }
                            Self::show_auto_review_order(ui, review);
                        });
                });
            review.open = open;
            if !review.open {
                close_review = true;
            }
        }

        if let Some(review) = self.advanced_auto_review.as_mut() {
            Self::show_auto_candidate_preview(ctx, review);
        }
        if apply_clicked {
            self.apply_advanced_auto_review();
        } else if close_review {
            self.advanced_auto_review = None;
        }
    }

    pub(super) fn show_auto_review_groups(ui: &mut Ui, review: &mut AdvancedAutoReviewState) {
        let groups = review
            .candidates
            .groups
            .iter()
            .map(|group| (group.id, group.signature.clone(), group.item_ids.clone()))
            .collect::<Vec<_>>();
        for (group_id, signature, item_ids) in groups {
            let color = advanced_group_color(group_id);
            let removed = review.removed_groups.contains(&group_id);
            Frame::group(ui.style())
                .stroke(Stroke::new(2.0, color))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(tf!("launcher.new_project.group_images_count", group_id = group_id + 1, item_ids = item_ids.len()))
                            .strong(),
                        );
                        ui.label(RichText::new(signature).small().weak());
                        let label = if removed {
                            t!("launcher.new_project.restore_group_button")
                        } else {
                            t!("launcher.new_project.delete_group_button")
                        };
                        if ui.button(label).clicked() {
                            if removed {
                                review.removed_groups.remove(&group_id);
                            } else {
                                review.removed_groups.insert(group_id);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    Self::show_auto_candidate_cards(ui, review, &item_ids);
                });
            ui.add_space(8.0);
        }
    }

    pub(super) fn show_auto_review_order(ui: &mut Ui, review: &mut AdvancedAutoReviewState) {
        let mut item_ids = review
            .candidates
            .items
            .iter()
            .map(|item| (item.order_index, item.id))
            .collect::<Vec<_>>();
        item_ids.sort_by_key(|(order_index, _)| *order_index);
        let ids = item_ids
            .into_iter()
            .map(|(_, item_id)| item_id)
            .collect::<Vec<_>>();
        Self::show_auto_candidate_cards(ui, review, &ids);
    }

    pub(super) fn show_auto_candidate_cards(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_ids: &[usize],
    ) {
        let (columns, card_side) = auto_review_card_layout(ui);
        let card_step = card_side + AUTO_REVIEW_CARD_GAP;
        for (row_index, row) in item_ids.chunks(columns).enumerate() {
            let row_width = row
                .iter()
                .enumerate()
                .fold(0.0_f32, |width, (column_index, _)| {
                    if column_index == 0 {
                        card_side
                    } else {
                        width + card_step
                    }
                });
            let (row_rect, _) =
                ui.allocate_exact_size(egui::vec2(row_width, card_side), egui::Sense::hover());
            for (column_index, &item_id) in row.iter().enumerate() {
                let x = row_rect.left() + card_step * auto_review_index_as_f32(column_index);
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x, row_rect.top()),
                    egui::vec2(card_side, card_side),
                );
                let card_id =
                    ui.id()
                        .with(("advanced_auto_card", row_index, column_index, item_id));
                Self::show_auto_candidate_card(ui, review, item_id, rect, card_id);
            }
            ui.add_space(AUTO_REVIEW_CARD_GAP);
        }
    }

    pub(super) fn show_auto_candidate_card(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_id: usize,
        rect: egui::Rect,
        card_id: egui::Id,
    ) {
        let Some((group_id, order_index, width, height, url)) = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| {
                (
                    item.group_id,
                    item.order_index,
                    item.width,
                    item.height,
                    item.url.clone(),
                )
            })
        else {
            return;
        };

        let removed =
            review.removed_items.contains(&item_id) || review.removed_groups.contains(&group_id);
        let stroke_color = if removed {
            egui::Color32::from_gray(80)
        } else {
            advanced_group_color(group_id)
        };
        let fill = ui.visuals().widgets.noninteractive.bg_fill;
        let response = ui.interact(rect, card_id, egui::Sense::click());
        let parent_clip = ui.clip_rect();
        let card_clip = parent_clip.intersect(rect);
        ui.painter().with_clip_rect(card_clip).rect(
            rect,
            egui::CornerRadius::same(4),
            fill,
            Stroke::new(2.0, stroke_color),
            egui::StrokeKind::Inside,
        );
        if response.clicked() && !removed {
            review.expanded_item = Some(item_id);
        }

        let inner = rect.shrink(AUTO_REVIEW_CARD_MARGIN);
        let header_rect = egui::Rect::from_min_size(
            inner.min,
            egui::vec2(inner.width(), AUTO_REVIEW_CARD_HEADER_HEIGHT),
        );
        let footer_rect = egui::Rect::from_min_size(
            egui::pos2(
                inner.left(),
                inner.bottom() - AUTO_REVIEW_CARD_FOOTER_HEIGHT,
            ),
            egui::vec2(inner.width(), AUTO_REVIEW_CARD_FOOTER_HEIGHT),
        );
        let image_rect = egui::Rect::from_min_max(
            egui::pos2(inner.left(), header_rect.bottom() + 4.0),
            egui::pos2(inner.right(), footer_rect.top() - 6.0),
        );

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(header_rect)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.set_clip_rect(parent_clip.intersect(header_rect));
                ui.label(RichText::new(format!("#{}", order_index + 1)).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if review.removed_items.contains(&item_id) {
                        t!("launcher.new_project.restore_button")
                    } else {
                        "×"
                    };
                    if ui.small_button(label).clicked() {
                        if review.removed_items.contains(&item_id) {
                            review.removed_items.remove(&item_id);
                        } else {
                            review.removed_items.insert(item_id);
                        }
                    }
                });
            },
        );

        if let Some(texture) = Self::auto_thumb_texture(ui, review, item_id) {
            let response = ui.put(
                image_rect,
                egui::Image::new((texture.id(), image_rect.size())).sense(egui::Sense::click()),
            );
            if response.clicked() && !removed {
                review.expanded_item = Some(item_id);
            }
        }
        if removed {
            ui.painter()
                .with_clip_rect(parent_clip.intersect(image_rect))
                .rect_filled(
                    image_rect,
                    egui::CornerRadius::same(3),
                    egui::Color32::from_black_alpha(150),
                );
        }

        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(footer_rect)
                .layout(Layout::top_down(Align::Min)),
            |ui| {
                ui.set_clip_rect(parent_clip.intersect(footer_rect));
                ui.label(RichText::new(format!("{width}×{height}")).small());
                ui.label(RichText::new(shorten_url(&url, 31)).small().weak());
            },
        );
    }

    pub(super) fn auto_thumb_texture(
        ui: &mut Ui,
        review: &mut AdvancedAutoReviewState,
        item_id: usize,
    ) -> Option<TextureHandle> {
        if let Some(texture) = review.thumb_textures.get(&item_id) {
            return Some(texture.clone());
        }
        let thumbnail = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| item.thumbnail.clone())?;
        let texture = ui.ctx().load_texture(
            format!("advanced-auto-thumb-{item_id}"),
            thumbnail,
            TextureOptions::LINEAR,
        );
        review.thumb_textures.insert(item_id, texture.clone());
        Some(texture)
    }

    pub(super) fn show_auto_candidate_preview(ctx: &egui::Context, review: &mut AdvancedAutoReviewState) {
        let Some(item_id) = review.expanded_item else {
            return;
        };
        let Some((width, height, url, image)) = review
            .candidates
            .items
            .iter()
            .find(|item| item.id == item_id)
            .map(|item| {
                (
                    item.width,
                    item.height,
                    item.url.clone(),
                    item.image.clone(),
                )
            })
        else {
            review.expanded_item = None;
            return;
        };
        if review
            .expanded_texture
            .as_ref()
            .is_none_or(|(texture_item_id, _)| *texture_item_id != item_id)
        {
            let preview = dynamic_image_preview(&image, 900, 680);
            let texture = ctx.load_texture(
                format!("advanced-auto-preview-{item_id}"),
                preview,
                TextureOptions::LINEAR,
            );
            review.expanded_texture = Some((item_id, texture));
        }
        let mut open = true;
        Window::new(t!("launcher.new_project.image_view_title")).id(egui::Id::new("launcher.new_project.image_view_title"))
            .open(&mut open)
            .resizable(true)
            .default_width(960.0)
            .default_height(760.0)
            .show(ctx, |ui| {
                ui.label(format!("{width}×{height}"));
                ui.label(RichText::new(url).small().weak());
                ui.horizontal(|ui| {
                    if ui.button(t!("launcher.common.delete_button")).clicked() {
                        review.removed_items.insert(item_id);
                    }
                    if ui.button(t!("launcher.new_project.keep_button")).clicked() {
                        review.removed_items.remove(&item_id);
                    }
                });
                ui.separator();
                if let Some((_, texture)) = &review.expanded_texture {
                    let size = texture.size_vec2();
                    ScrollArea::both()
                        .id_salt("advanced_auto_preview_scroll")
                        .show(ui, |ui| {
                            ui.add(egui::Image::new((texture.id(), size)));
                        });
                }
            });
        if !open {
            review.expanded_item = None;
            review.expanded_texture = None;
        }
    }

    pub(super) fn apply_advanced_auto_review(&mut self) {
        let Some(review) = self.advanced_auto_review.as_ref() else {
            return;
        };
        let pages = match build_pages_from_auto_candidates(
            &review.candidates,
            &review.removed_items,
            &review.removed_groups,
        ) {
            Ok(pages) => pages,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.autofind_no_images_for_ribbon").to_string();
                return;
            }
        };
        let source_url = review.candidates.source_url.clone();
        let page_count = pages.len();
        self.ribbon
            .replace_source(PathBuf::from(&source_url), pages);
        self.selected_ribbon_page = self.default_selected_page();
        self.crop_editor = None;
        self.manual_cut_guides.clear();
        self.active_progress = None;
        self.last_error = None;
        self.simple_stitch_done = false;
        self.simple_manual_cut_preview_active = false;
        self.advance_simple_import_step_after_success();
        self.import_status =
            tf!("launcher.new_project.autofind_added_status", page_count = page_count, source_url = source_url);
        self.advanced_auto_review = None;
    }

    pub(super) fn clamp_advanced_indexes(&mut self) {
        if self.selected_browser >= self.browser_names.len() {
            self.selected_browser = 0;
        }
        if self.selected_site >= self.site_presets.len() {
            self.selected_site = 0;
        }
    }

    pub(super) fn advanced_browser_available(&self) -> bool {
        match self.selected_advanced_backend {
            AdvancedBrowserBackend::Selenium => !self.browser_names.is_empty(),
            AdvancedBrowserBackend::Cloak => true,
        }
    }

    pub(super) fn selected_browser_name(&self) -> Option<String> {
        match self.selected_advanced_backend {
            AdvancedBrowserBackend::Selenium => {
                self.browser_names.get(self.selected_browser).cloned()
            }
            AdvancedBrowserBackend::Cloak => Some(
                self.advanced_download
                    .browser_name_for_backend("CloakBrowser"),
            ),
        }
    }

    pub(super) fn start_advanced_open(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        if self.advanced_page_url.trim().is_empty() {
            self.last_error = Some(t!("launcher.new_project.enter_page_link_error").to_string());
            self.import_status = t!("launcher.new_project.advanced_dl_waiting_link").to_string();
            return;
        }
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.opening_page_selenium_status").to_string();
        self.advanced_download
            .begin_open(browser, self.advanced_page_url.clone());
    }

    pub(super) fn start_advanced_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "collect".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.collecting_links_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download
                .begin_fetch_auto(browser, self.advanced_fetch_parallelism);
        } else {
            self.advanced_download.begin_fetch(
                browser,
                self.image_prefix.trim().to_string(),
                self.advanced_fetch_parallelism,
            );
        }
    }

    pub(super) fn stop_advanced_auto_fetch(&mut self) {
        match self.advanced_download.request_cancel_current_auto_fetch() {
            Ok(()) => {
                self.import_status =
                    t!("launcher.new_project.stopping_autofind_status").to_string();
            }
            Err(err) => {
                self.last_error = Some(t!("launcher.new_project.stop_autofind_error").to_string());
                ms_log::runtime_log::log_warn(format!(
                    "[new-project] failed to request advanced auto fetch cancellation: {err}"
                ));
            }
        }
    }

    pub(super) fn start_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_link_collect_found_links = 0;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.starting_bg_collect_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download
                .begin_start_auto_link_collect(browser, self.advanced_fetch_parallelism);
        } else {
            self.advanced_download.begin_start_link_collect(
                browser,
                self.image_prefix.trim().to_string(),
                self.advanced_fetch_parallelism,
            );
        }
    }

    pub(super) fn finish_advanced_link_collect(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "download".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_collect_status").to_string();
        if self.advanced_link_source_mode == AdvancedLinkSourceMode::AutoReview {
            self.advanced_download.begin_stop_auto_link_collect(browser);
        } else {
            self.advanced_download.begin_stop_link_collect(browser);
        }
    }

    pub(super) fn start_advanced_canvas_fetch(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.advanced_intercept_counts = InterceptCounts::default();
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "collect_canvas".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.collecting_canvas_status").to_string();
        self.advanced_download.begin_fetch_canvas(browser);
    }

    pub(super) fn start_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status = t!("launcher.new_project.starting_canvas_intercept_status").to_string();
        self.advanced_download.begin_start_canvas_intercept(browser);
    }

    pub(super) fn finish_advanced_canvas_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.no_supported_browser_error").to_string());
            self.import_status = t!("launcher.new_project.selenium_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "save_canvas".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_canvas_intercept_status").to_string();
        self.advanced_download.begin_stop_canvas_intercept(browser);
    }

    /// Forces the deep-capture preconditions for the simple-mode
    /// "Автоматический перехват картинок" section, whose buttons have no
    /// backend/mode selectors: Cloak backend on both the UI state and the
    /// controller, and `DeepCapture` mode so the count poller queries the
    /// deep-intercept counters. `set_backend` is a no-op when already Cloak.
    pub(super) fn prepare_simple_deep_capture(&mut self) {
        self.selected_advanced_backend = AdvancedBrowserBackend::Cloak;
        self.advanced_mode = AdvancedDownloadMode::DeepCapture;
        self.advanced_download
            .set_backend(AdvancedBrowserBackend::Cloak);
    }

    pub(super) fn start_advanced_deep_intercept(&mut self) {
        if self.selected_advanced_backend != AdvancedBrowserBackend::Cloak {
            self.last_error =
                Some(t!("launcher.new_project.deep_intercept_cloak_only_error").to_string());
            return;
        }
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.cloak_unavailable_error").to_string());
            self.import_status = t!("launcher.new_project.deep_intercept_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "browser".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.starting_deep_intercept_status").to_string();
        self.advanced_download.begin_start_deep_intercept(browser);
    }

    pub(super) fn finish_advanced_deep_intercept(&mut self) {
        let Some(browser) = self.selected_browser_name() else {
            self.last_error = Some(t!("launcher.new_project.cloak_unavailable_error").to_string());
            self.import_status = t!("launcher.new_project.deep_intercept_browser_unavailable").to_string();
            return;
        };
        self.last_error = None;
        self.active_progress = Some(OperationProgress {
            operation: "advanced_download",
            stage: "download".to_string(),
            current: 0,
            total: 0,
        });
        self.import_status =
            t!("launcher.new_project.stopping_deep_intercept_status").to_string();
        self.advanced_download.begin_stop_deep_intercept(browser);
    }

    pub(super) fn save_advanced_prefix(&mut self) {
        let prefix = self.image_prefix.trim().to_string();
        let new_name = self.site_name.trim().to_string();
        let selected_name = self
            .site_presets
            .get(self.selected_site)
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        if prefix.is_empty() {
            self.last_error = Some(t!("launcher.new_project.enter_url_prefix_error").to_string());
            self.import_status = t!("launcher.new_project.prefix_not_specified_error").to_string();
            return;
        }
        let target_name = if new_name.is_empty() {
            selected_name
        } else {
            new_name
        };
        if target_name.trim().is_empty() {
            self.last_error =
                Some(t!("launcher.new_project.enter_site_name_error").to_string());
            self.import_status = t!("launcher.new_project.save_prefix_failed").to_string();
            return;
        }

        match save_image_url_preset(&target_name, &prefix) {
            Ok(()) => {
                self.site_presets = load_image_url_presets();
                self.selected_site = self
                    .site_presets
                    .iter()
                    .position(|(name, _)| *name == target_name)
                    .unwrap_or(0);
                self.site_name.clear();
                self.last_error = None;
                self.import_status =
                    tf!("launcher.new_project.prefix_saved_status", target_name = target_name.trim());
            }
            Err(err) => {
                ms_log::runtime_log::log_error(format!(
                    "[new-project] failed to save image url preset '{target_name}': {err}"
                ));
                self.last_error =
                    Some(t!("launcher.new_project.save_prefix_config_error").to_string());
                self.import_status = t!("launcher.new_project.save_prefix_failed").to_string();
            }
        }
    }
}

pub(super) fn advanced_group_color(group_id: usize) -> egui::Color32 {
    const COLORS: [egui::Color32; 10] = [
        egui::Color32::from_rgb(244, 67, 54),
        egui::Color32::from_rgb(33, 150, 243),
        egui::Color32::from_rgb(76, 175, 80),
        egui::Color32::from_rgb(255, 193, 7),
        egui::Color32::from_rgb(156, 39, 176),
        egui::Color32::from_rgb(0, 188, 212),
        egui::Color32::from_rgb(255, 112, 67),
        egui::Color32::from_rgb(139, 195, 74),
        egui::Color32::from_rgb(121, 85, 72),
        egui::Color32::from_rgb(96, 125, 139),
    ];
    COLORS[group_id % COLORS.len()]
}

pub(super) fn auto_review_card_layout(ui: &Ui) -> (usize, f32) {
    let available_width = ui.available_width().min(ui.clip_rect().width()).max(1.0);
    let mut columns = 1usize;
    loop {
        let next_columns = columns.saturating_add(1);
        let gap_width = auto_review_gap_width(next_columns);
        let next_width = ((available_width - gap_width) / auto_review_columns_as_f32(next_columns))
            .floor()
            .min(AUTO_REVIEW_CARD_SIDE);
        if next_width < AUTO_REVIEW_CARD_MIN_SIDE {
            break;
        }
        columns = next_columns;
    }
    let gap_width = auto_review_gap_width(columns);
    let card_side = if columns == 1 {
        available_width.floor().clamp(1.0, AUTO_REVIEW_CARD_SIDE)
    } else {
        ((available_width - gap_width) / auto_review_columns_as_f32(columns))
            .floor()
            .clamp(AUTO_REVIEW_CARD_MIN_SIDE, AUTO_REVIEW_CARD_SIDE)
    };
    (columns, card_side)
}

pub(super) fn auto_review_gap_width(columns: usize) -> f32 {
    (1..columns).fold(0.0, |width, _| width + AUTO_REVIEW_CARD_GAP)
}

pub(super) fn auto_review_columns_as_f32(columns: usize) -> f32 {
    (0..columns).fold(0.0_f32, |count, _| count + 1.0).max(1.0)
}

pub(super) fn auto_review_index_as_f32(index: usize) -> f32 {
    (0..index).fold(0.0_f32, |count, _| count + 1.0)
}

pub(super) fn auto_review_default_removed_groups(candidates: &AdvancedAutoCandidateSet) -> HashSet<usize> {
    candidates
        .groups
        .iter()
        .filter(|group| group.item_ids.len() <= 1)
        .map(|group| group.id)
        .collect()
}

/// Candidate ids the review opens with unchecked: helper-flagged probable junk
/// (size-outlier icons, sprites, UI chrome), so the user keeps the real pages.
pub(super) fn auto_review_default_removed_items(candidates: &AdvancedAutoCandidateSet) -> HashSet<usize> {
    candidates
        .items
        .iter()
        .filter(|item| item.probable_junk)
        .map(|item| item.id)
        .collect()
}

pub(super) fn shorten_url(url: &str, limit: usize) -> String {
    if url.chars().count() <= limit {
        return url.to_string();
    }
    let mut value = url
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    value.push('…');
    value
}

pub(super) fn dynamic_image_preview(
    image: &DynamicImage,
    max_width: u32,
    max_height: u32,
) -> egui::ColorImage {
    let preview = image.thumbnail(max_width, max_height).to_rgba8();
    let width = usize::try_from(preview.width()).unwrap_or(1).max(1);
    let height = usize::try_from(preview.height()).unwrap_or(1).max(1);
    egui::ColorImage::from_rgba_unmultiplied([width, height], preview.as_raw())
}
