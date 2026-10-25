/*
File: tools/watermark_removal/editor_ui.rs

Purpose:
The tool's egui UI: the mode entry and every `draw_*` section of the region editor and the
chapter panel, drawn through `WatermarkEditorCtx`.

Key items:
- `draw_mode_entry()`
- `impl WatermarkEditorCtx<'_>`

Notes:
GUI thread only; no blocking work. `WatermarkEditorCtx` is declared in `mod.rs`.
*/
use crate::tools::base::{RegionEditToolBase, RegionEditorSession};
use crate::tools::mask_generation::{
    draw_watermark_model_picker_ui, draw_watermark_progress_ui, watermark_model_spec,
};
use crate::watermark_chapter::WatermarkModel;
use ms_widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use egui::{Color32, Pos2, Rect, TextureOptions};
use super::{
    CHAPTER_PREVIEW_SIDE, ChapterMark, ChapterPreviewTextures, ChapterRequest, ChapterState,
    WATERMARK_DILATE_MAX, WATERMARK_OVERLAP_MAX, WATERMARK_THRESHOLD_MAX, WATERMARK_THRESHOLD_MIN,
    WATERMARK_TILE_MAX, WATERMARK_TILE_MIN, WatermarkEditorCtx, WatermarkMode,
    describe_conditioning,
};
use super::describe::{
    describe_histogram, describe_residual, describe_sample_verdict, library_quality_label,
};
use super::settings::{effective_watermark_mode, offered_watermark_modes};


/// Draws one entry of the mode dropdown and applies a click to `selected`.
///
/// A Torch-backed mode is disabled while the backend reports no Torch and explains that on
/// hover; the AI-free chapter mode is always selectable. Deliberately the same shape as the
/// shared `mask_generation::draw_mask_source_entry`, down to the message and its colour, so
/// the two Torch gates of this tab cannot word the same refusal differently.
fn draw_mode_entry(
    ui: &mut egui::Ui,
    selected: &mut WatermarkMode,
    candidate: WatermarkMode,
    torch_available: bool,
) {
    let enabled = torch_available || !candidate.requires_torch();
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(candidate.label()).selected(*selected == candidate),
    );
    let response = if enabled {
        response
    } else {
        response.on_disabled_hover_text(
            egui::RichText::new(t!("cleaning.common.pytorch_not_installed_status"))
                .color(ms_theme::status::ERROR),
        )
    };
    if response.clicked() {
        *selected = candidate;
    }
}

impl WatermarkEditorCtx<'_> {
    /// The mode this session runs in; see `effective_watermark_mode`.
    fn effective_mode(&self) -> WatermarkMode {
        effective_watermark_mode(&self.settings.mode, self.series_watermark_library)
    }

    /// Draws the whole region-editor body: scrollable controls plus preview, then
    /// the run/undo action row. The status line and Отмена/Применить are appended
    /// by `RegionEditToolBase::draw_overlay_ui`.
    pub(super) fn draw_body(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        self.session.sync_session(editor.scroll_id);
        let running = self.session.poll_run(editor);
        let mode = self.effective_mode();

        let scroll_id = editor.scroll_id;
        // Keep the action row fixed while a long parameter list scrolls, and keep
        // mouse-drag out of the scroll sources so dragging the preview does not
        // scroll the panel.
        let scroll_max_h = (ui.ctx().content_rect().height() - 200.0).max(240.0);
        egui::ScrollArea::vertical()
            .id_salt(("cleaning_watermark_body_scroll", scroll_id))
            .max_height(scroll_max_h)
            .auto_shrink([false, true])
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if RegionEditToolBase::draw_region_editor_zoom_controls(ui, editor) {
                        ui.ctx().request_repaint();
                    }
                });
                // The mode picker stays outside the collapsible sections: it decides which
                // of them is drawn at all.
                self.draw_mode_picker(ui);
                match mode {
                    WatermarkMode::MaskOnly | WatermarkMode::Clean => {
                        draw_watermark_progress_ui(ui, self.progress);
                        self.draw_params(ui, scroll_id);
                    }
                    WatermarkMode::Chapter => {
                        self.draw_chapter_progress(ui);
                        self.draw_chapter_params(ui, scroll_id);
                        self.draw_chapter_catalog(ui, editor, scroll_id);
                        self.draw_chapter_reports(ui);
                        self.draw_chapter_library(ui, scroll_id);
                    }
                }
                self.draw_preview(ui, editor);
            });

        ui.separator();
        match mode {
            WatermarkMode::MaskOnly | WatermarkMode::Clean => {
                self.draw_actions(ui, editor, running)
            }
            WatermarkMode::Chapter => self.draw_chapter_actions(ui, editor),
        }
        if running || self.chapter.busy() {
            ui.ctx().request_repaint();
        }
    }

    /// Draws the three-way mode picker. The chapter mode is local, so it also states that
    /// it needs no backend at all.
    ///
    /// The two NETWORK modes are disabled while the backend reports no Torch and say so on
    /// hover; «По главе (точное вычитание)» is always selectable. A persisted selection is
    /// never forced to change by this — a settings file naming a network mode still opens,
    /// with the run button explaining why it cannot run.
    fn draw_mode_picker(&mut self, ui: &mut egui::Ui) {
        let effective = self.effective_mode();
        let mut mode = effective;
        let torch_available = self.ai_backend_torch_available;
        let offered = offered_watermark_modes(self.series_watermark_library);
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.common.mode_label"));
            WheelComboBox::from_id_salt("cleaning_watermark_mode_picker")
                .selected_text(mode.label())
                .show_ui(ui, |ui| {
                    for &candidate in offered {
                        draw_mode_entry(ui, &mut mode, candidate, torch_available);
                    }
                });
        });
        // Compared with the EFFECTIVE mode, not the persisted string: a single-image session
        // shows a persisted «По главе» as «Только маска», and only a real pick may overwrite it.
        if mode != effective {
            self.settings.mode = mode.wire().to_string();
            *self.settings_changed = true;
        }
        match mode {
            WatermarkMode::MaskOnly => {}
            WatermarkMode::Clean => {
                ui.colored_label(
                    ms_theme::status::WARNING,
                    t!("cleaning.tools.watermark.experimental_warning"),
                );
            }
            WatermarkMode::Chapter => {
                ui.small(t!("cleaning.tools.watermark.chapter.description_hint"));
            }
        }
    }

    /// Draws the collapsed-by-default «Параметры (удаление водяных знаков)»
    /// section: model, mode, tiling, mask parameters, preview toggle and the
    /// backend catalog/unload controls.
    fn draw_params(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let settings = &mut *self.settings;
        let unload_status = &mut *self.unload_status;
        let changed = &mut *self.settings_changed;
        let want_status = &mut *self.want_status;
        let unload_requested = &mut *self.unload_requested;
        let status = self.status;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_params", scroll_id),
            t!("cleaning.tools.watermark.params_heading"),
            false,
            |ui| {
                // The model id is the persisted identity, so the picker works on
                // the catalog's `&'static str` and the settings string follows it.
                let mut selected_model = watermark_model_spec(&settings.model).id;
                draw_watermark_model_picker_ui(ui, &mut selected_model, status);
                if selected_model != settings.model {
                    settings.model = selected_model.to_string();
                    *changed = true;
                }

                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.tile, WATERMARK_TILE_MIN..=WATERMARK_TILE_MAX)
                            .text(t!("cleaning.tools.watermark.tile_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.tile_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.overlap, 0..=WATERMARK_OVERLAP_MAX)
                            .text(t!("cleaning.tools.watermark.overlap_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.overlap_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(
                            &mut settings.threshold,
                            WATERMARK_THRESHOLD_MIN..=WATERMARK_THRESHOLD_MAX,
                        )
                        .text(t!("cleaning.tools.watermark.threshold_label")),
                    )
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.dilate_px, 0..=WATERMARK_DILATE_MAX)
                            .text(t!("cleaning.common.mask_expand_label")),
                    )
                    .changed();
                *changed |= ui
                    .checkbox(
                        &mut settings.show_mask_preview,
                        t!("cleaning.tools.watermark.show_mask_label"),
                    )
                    .changed();

                if ui
                    .small_button(t!("cleaning.tools.watermark.refresh_status_button"))
                    .clicked()
                {
                    *want_status = true;
                }
                if ui
                    .small_button(t!("cleaning.tools.watermark.unload_button"))
                    .clicked()
                {
                    *unload_requested = true;
                }
                if let Some(status) = unload_status.as_ref() {
                    ui.small(status);
                }
            },
        );
    }

    /// Draws the region image with the predicted mask painted over it when the
    /// preview toggle is on.
    fn draw_preview(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        RegionEditToolBase::ensure_region_editor_texture(editor, ui.ctx());
        self.session.ensure_mask_texture(ui.ctx(), editor.scroll_id);
        // Cloned so the draw closure does not borrow `self` while `editor` is
        // borrowed; a `TextureHandle` clone is a refcount bump.
        let mask_texture = self
            .settings
            .show_mask_preview
            .then(|| self.session.mask_texture.clone())
            .flatten();
        let preview_size = editor.zoomed_image_size();
        let scroll_id = editor.scroll_id;
        RegionEditToolBase::draw_region_editor_scroll_area(ui, scroll_id, preview_size, |ui| {
            let Some(texture) = editor.texture.as_ref() else {
                return;
            };
            let response = ui.add(egui::Image::new((texture.id(), preview_size)));
            if let Some(mask_texture) = mask_texture.as_ref() {
                ui.painter().image(
                    mask_texture.id(),
                    response.rect,
                    Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
        });
    }

    /// Draws the chapter job's progress bar, if one is running.
    fn draw_chapter_progress(&mut self, ui: &mut egui::Ui) {
        let Some((done, total, label)) = self.chapter.progress.clone() else {
            return;
        };
        // Cast justification: both are page counts of one chapter, far below 2^24.
        let fraction = if total > 0 {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.add(egui::ProgressBar::new(fraction).text(tf!(
            "cleaning.tools.watermark.chapter.job_progress",
            label = label,
            done = done,
            total = total
        )));
        ui.ctx().request_repaint();
    }

    /// Draws the collapsed-by-default «Параметры (по главе)» section.
    ///
    /// Only the three parameters a real source can legitimately need are exposed. The gain
    /// window is NOT among them: the plan's measurement is that a source with fat solid
    /// glyphs needs a larger background radius, never a looser gain window, and the engine
    /// clamps every value handed to it to its own measured bounds anyway.
    fn draw_chapter_params(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let settings = &mut *self.settings;
        let changed = &mut *self.settings_changed;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_params", scroll_id),
            t!("cleaning.tools.watermark.chapter.params_heading"),
            false,
            |ui| {
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_anchor_tolerance_px, 0..=16)
                            .text(t!("cleaning.tools.watermark.chapter.anchor_tolerance_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.anchor_tolerance_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_background_blur_px, 1..=32)
                            .text(t!("cleaning.tools.watermark.chapter.blur_radius_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.blur_radius_hint"))
                    .changed();
                *changed |= ui
                    .add(
                        WheelSlider::new(&mut settings.chapter_ring_width_px, 1..=16)
                            .text(t!("cleaning.tools.watermark.chapter.ring_width_label")),
                    )
                    .on_hover_text(t!("cleaning.tools.watermark.chapter.ring_width_hint"))
                    .changed();
            },
        );
    }

    /// Draws the catalog of marks: selection, name, per-mark measurements, verdict and the
    /// model preview.
    ///
    /// The whole section is read-only while a job runs, because a worker owns a copy of the
    /// catalog and will hand back its own version when it finishes.
    fn draw_chapter_catalog(
        &mut self,
        ui: &mut egui::Ui,
        editor: &mut RegionEditorSession,
        scroll_id: u64,
    ) {
        let busy = self.chapter.busy();
        let chapter = &mut *self.chapter;
        let request = &mut *self.chapter_request;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_marks", scroll_id),
            t!("cleaning.tools.watermark.chapter.marks_heading"),
            true,
            |ui| {
                if chapter.catalog.is_empty() {
                    ui.small(t!("cleaning.tools.watermark.chapter.no_marks_hint"));
                    return;
                }
                let mut remove: Option<usize> = None;
                for index in 0..chapter.catalog.len() {
                    ui.push_id(index, |ui| {
                        ui.separator();
                        ui.horizontal(|ui| {
                            let selected = chapter.selected == index;
                            if ui
                                .add_enabled(!busy, egui::RadioButton::new(selected, ""))
                                .clicked()
                            {
                                chapter.selected = index;
                            }
                            ui.add_enabled_ui(!busy, |ui| {
                                // The name is user data: whatever is typed is kept verbatim.
                                ui.add(
                                    egui::TextEdit::singleline(
                                        &mut chapter.catalog.marks[index].name,
                                    )
                                    .id_salt(("cleaning_watermark_chapter_mark_name", index))
                                    .desired_width(200.0),
                                );
                            });
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!(
                                        "cleaning.tools.watermark.chapter.library_save_button"
                                    )),
                                )
                                .clicked()
                            {
                                *request = Some(ChapterRequest::SaveLibrary(index));
                            }
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t!(
                                        "cleaning.tools.watermark.chapter.remove_mark_button"
                                    )),
                                )
                                .clicked()
                            {
                                remove = Some(index);
                            }
                        });
                        Self::draw_chapter_mark_body(ui, chapter, index, busy, request);
                    });
                }
                if let Some(index) = remove {
                    // Drop the mark's preview textures with it, or the map would keep one
                    // entry per mark ever created in this session.
                    if let Some(kind) = chapter.catalog.kinds.get(index) {
                        let id = kind.id().to_string();
                        chapter.textures.remove(&id);
                    }
                    chapter.catalog.remove(index);
                    chapter.selected = chapter.selected.min(chapter.catalog.len().saturating_sub(1));
                    chapter.invalidate_scan();
                }
            },
        );
        // The editor's status line doubles as the chapter status line while this mode is
        // active, so a finished job is visible without hunting for the tool panel.
        if let Some(status) = chapter.status.as_ref() {
            editor.status = Some(status.clone());
        }
    }

    /// Draws one mark's measurements, verdict, suggested fix, library match and preview.
    fn draw_chapter_mark_body(
        ui: &mut egui::Ui,
        chapter: &mut ChapterState,
        index: usize,
        busy: bool,
        request: &mut Option<ChapterRequest>,
    ) {
        let Some(kind) = chapter.catalog.kinds.get(index) else {
            return;
        };
        // A mark rebuilt from a library entry whose background was hand-stated keeps the
        // verdict the engine gave it, so the assertion has to be read off the model's
        // provenance: without it this editor would claim a measurement the card warns about.
        let rests_on_assertion = kind
            .model()
            .is_some_and(|model| model.provenance().manual_backgrounds > 0);
        let conditioning_lines = describe_conditioning(kind.conditioning(), rests_on_assertion);
        let noise_gain = kind.model().map(WatermarkModel::max_noise_gain);
        let template_size = (kind.template().width(), kind.template().height());
        let anchors = kind.template().anchor_key();
        let calibration_samples = kind.samples().len();
        let Some(mark) = chapter.catalog.marks.get(index) else {
            return;
        };
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.mark_stats",
            width = template_size.0,
            height = template_size.1,
            calibration = calibration_samples,
            template_only = mark.template_only,
            occurrences = mark.occurrences
        ));
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.anchors_line",
            anchors = anchors
        ));
        for line in conditioning_lines {
            ui.small(line);
        }
        if let Some(gain) = noise_gain {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.noise_gain_line",
                gain = format!("{gain:.2}")
            ));
        }
        if mark.occurrences > 0 {
            // Model quality is the DETECTION evidence — gain and t — never the
            // recomposition residual, which only measures quantization and clipping.
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.quality_line",
                gain = format!("{:.2}", mark.gain_mean),
                snr = format!("{:.0}", mark.snr_mean),
                count = mark.occurrences - mark.unverified
            ));
            if mark.unverified > 0 {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.unverified_line",
                    count = mark.unverified
                ));
            }
            let histogram = describe_histogram(&mark.histogram);
            if !histogram.is_empty() {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.histogram_line",
                    bins = histogram
                ));
            }
        }
        if let Some(verdict) = mark.last_verdict.as_ref() {
            ui.small(describe_sample_verdict(verdict));
        }
        Self::draw_chapter_match_picker(ui, mark, index, busy, request);
        Self::draw_chapter_preview(ui, chapter, index);
    }

    /// Draws the library auto-match row of one mark: which entry supplies its calibration,
    /// and the override that lets the user pick another one or none at all.
    ///
    /// Matching is shape-independent by design, so the row states the EVIDENCE — the entry's
    /// calibration quality and the measured opacity gain against this mark's own deposit —
    /// rather than just a name.
    fn draw_chapter_match_picker(
        ui: &mut egui::Ui,
        mark: &ChapterMark,
        index: usize,
        busy: bool,
        request: &mut Option<ChapterRequest>,
    ) {
        if mark.matches.is_empty() {
            return;
        }
        let current = mark.adopted_entry.clone();
        let selected_text = match current.as_ref() {
            Some(entry_id) => mark
                .matches
                .iter()
                .find(|candidate| &candidate.entry_id == entry_id)
                .map_or_else(|| entry_id.clone(), |candidate| candidate.name.clone()),
            None => t!("cleaning.tools.watermark.chapter.library_match_none").to_string(),
        };
        let mut chosen: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.small(t!("cleaning.tools.watermark.chapter.library_match_label"));
            // Read-only while a worker owns a copy of the catalog: it will hand back its own
            // version, and a choice made against this one would be thrown away.
            ui.add_enabled_ui(!busy, |ui| {
                WheelComboBox::from_id_salt(("cleaning_watermark_chapter_match", index))
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                current.is_none(),
                                t!("cleaning.tools.watermark.chapter.library_match_none"),
                            )
                            .clicked()
                        {
                            chosen = Some(None);
                        }
                        for candidate in &mark.matches {
                            let label = tf!(
                                "cleaning.tools.watermark.chapter.library_match_option",
                                name = candidate.name.clone(),
                                quality = library_quality_label(&candidate.verdict),
                                gain = format!("{:.2}", candidate.gain)
                            );
                            if ui
                                .selectable_label(
                                    current.as_deref() == Some(candidate.entry_id.as_str()),
                                    label,
                                )
                                .clicked()
                            {
                                chosen = Some(Some(candidate.entry_id.clone()));
                            }
                        }
                    });
            });
        });
        if let Some(entry) = chosen
            && entry != current
        {
            *request = Some(ChapterRequest::UseLibraryMatch { index, entry });
        }
    }

    /// Draws the alpha-map and imprint thumbnails of a fitted model, uploading their
    /// textures on first use and whenever the model was refitted.
    fn draw_chapter_preview(ui: &mut egui::Ui, chapter: &mut ChapterState, index: usize) {
        let Some(id) = chapter.catalog.kinds.get(index).map(|kind| kind.id().to_string()) else {
            return;
        };
        let Some(mark) = chapter.catalog.marks.get(index) else {
            return;
        };
        let Some(preview) = mark.preview.as_ref() else {
            return;
        };
        let revision = mark.preview_revision;
        let stale = !matches!(chapter.textures.get(&id), Some(textures) if textures.revision == revision);
        if stale {
            let alpha = preview.alpha.clone();
            let imprint = preview.imprint.clone();
            let alpha = ui.ctx().load_texture(
                format!("cleaning-watermark-chapter-alpha-{id}"),
                alpha,
                TextureOptions::NEAREST,
            );
            let imprint = ui.ctx().load_texture(
                format!("cleaning-watermark-chapter-imprint-{id}"),
                imprint,
                TextureOptions::NEAREST,
            );
            chapter.textures.insert(
                id.clone(),
                ChapterPreviewTextures {
                    revision,
                    alpha,
                    imprint,
                },
            );
        }
        let Some(textures) = chapter.textures.get(&id) else {
            return;
        };
        let size = egui::vec2(CHAPTER_PREVIEW_SIDE, CHAPTER_PREVIEW_SIDE);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.small(t!("cleaning.tools.watermark.chapter.preview_alpha_label"));
                ui.add(egui::Image::new((textures.alpha.id(), size)));
            });
            ui.vertical(|ui| {
                ui.small(t!("cleaning.tools.watermark.chapter.preview_imprint_label"));
                ui.add(egui::Image::new((textures.imprint.id(), size)));
            });
        });
    }

    /// Draws the totals of the last scan and the last apply.
    fn draw_chapter_reports(&mut self, ui: &mut egui::Ui) {
        if let Some(report) = self.chapter.scan_report {
            ui.separator();
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.scan_done_status",
                found = report.found,
                pages = report.pages,
                unverified = report.unverified
            ));
            if report.matched > 0 {
                ui.small(tf!(
                    "cleaning.tools.watermark.chapter.library_matched_line",
                    count = report.matched
                ));
            }
        }
        if let Some(report) = self.chapter.apply_report {
            ui.small(tf!(
                "cleaning.tools.watermark.chapter.apply_done_status",
                pages = report.pages,
                removed = report.removed,
                refused = report.refused
            ));
            ui.small(describe_residual(&report.residual));
            if report.failed_patches > 0 {
                ui.colored_label(
                    ms_theme::status::WARNING,
                    tf!(
                        "cleaning.tools.watermark.chapter.apply_failed_patches",
                        count = report.failed_patches
                    ),
                );
            }
        }
    }

    /// Draws the library picker: refresh, the entry list and a load button per entry.
    fn draw_chapter_library(&mut self, ui: &mut egui::Ui, scroll_id: u64) {
        let busy = self.chapter.busy();
        let library_panel_open = self.library_panel_open;
        let chapter = &mut *self.chapter;
        let request = &mut *self.chapter_request;
        RegionEditToolBase::draw_region_editor_collapsible_section(
            ui,
            ("cleaning_watermark_chapter_library", scroll_id),
            t!("cleaning.tools.watermark.chapter.library_heading"),
            false,
            |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(t!(
                                "cleaning.tools.watermark.chapter.library_refresh_button"
                            )),
                        )
                        .clicked()
                    {
                        *request = Some(ChapterRequest::RefreshLibrary);
                    }
                    // A TOGGLE drawn pressed while the panel is open: a dock tab has no
                    // close affordance of its own, so this button is the whole story.
                    if ui
                        .add(
                            egui::Button::new(t!(
                                "cleaning.tools.watermark.chapter.library_manage_button"
                            ))
                            .selected(library_panel_open),
                        )
                        .on_hover_text(t!(
                            "cleaning.tools.watermark.chapter.library_manage_hint"
                        ))
                        .clicked()
                    {
                        *request = Some(ChapterRequest::ToggleLibraryPanel);
                    }
                });
                if chapter.library.is_empty() {
                    ui.small(t!("cleaning.tools.watermark.chapter.library_empty_hint"));
                    return;
                }
                for entry in &chapter.library {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_load_button"
                                )),
                            )
                            .clicked()
                        {
                            *request = Some(ChapterRequest::LoadLibrary(entry.id.clone()));
                        }
                        ui.small(tf!(
                            "cleaning.tools.watermark.chapter.library_entry_line",
                            name = entry.name.clone(),
                            width = entry.width,
                            height = entry.height,
                            anchors = entry.anchor_key.clone(),
                            samples = entry.samples
                        ));
                    });
                    if !entry.sources.is_empty() {
                        ui.small(tf!(
                            "cleaning.tools.watermark.chapter.library_sources_line",
                            sources = entry
                                .sources
                                .iter()
                                .map(|source| format!(
                                    "{} ({} px, {})",
                                    source.source_key, source.page_width, source.anchor_key
                                ))
                                .collect::<Vec<_>>()
                                .join("; ")
                        ));
                    }
                }
            },
        );
    }

    /// Draws the chapter action row: add a mark, add a sample, scan, apply.
    fn draw_chapter_actions(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession) {
        let busy = self.chapter.busy();
        let has_marks = !self.chapter.catalog.is_empty();
        let has_hits = !self.chapter.hits.is_empty();
        let selected = self.chapter.selected;
        let page_idx = editor.page_idx;
        let rect = editor.target_rect_px;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.add_mark_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.add_mark_hint"))
                .on_disabled_hover_text(t!(
                    "cleaning.mask_editor.processing_already_running_status"
                ))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Sample {
                    page_idx,
                    rect,
                    target: None,
                });
            }
            if ui
                .add_enabled(
                    !busy && has_marks,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.add_sample_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.add_sample_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Sample {
                    page_idx,
                    rect,
                    target: Some(selected),
                });
            }
            if ui
                .add_enabled(
                    !busy && has_marks,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.scan_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.scan_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Scan);
            }
            if ui
                .add_enabled(
                    !busy && has_hits,
                    egui::Button::new(t!("cleaning.tools.watermark.chapter.apply_button")),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.apply_hint"))
                .clicked()
            {
                *self.chapter_request = Some(ChapterRequest::Apply);
            }
            if busy {
                ui.spinner();
            }
        });
    }

    /// Draws the run / undo / cancel row. The run button names what the current
    /// mode will do and explains on hover why it is disabled.
    ///
    /// Both network modes need a reachable backend AND a Torch runtime. Since
    /// `CleaningTool::pytorch_required` is a constant `false` — the tool must stay selectable
    /// for its AI-free chapter mode — this is the gate that keeps a Torch-less machine from
    /// firing a request that can only fail. The hint is three-way in the same order as the
    /// shared `mask_generation::generate_button_hover_text`: backend first, Torch second,
    /// what the run does last.
    fn draw_actions(&mut self, ui: &mut egui::Ui, editor: &mut RegionEditorSession, running: bool) {
        let Some(mode) = self.effective_mode().network() else {
            return;
        };
        let backend_available = self.ai_backend_available;
        let torch_available = self.ai_backend_torch_available;
        let can_run = !running && backend_available && torch_available;
        let can_undo = !running && !self.session.undo_stack.is_empty();
        ui.horizontal(|ui| {
            let run_hint = if !backend_available {
                t!("cleaning.mask_editor.backend_unavailable_status")
            } else if !torch_available {
                t!("cleaning.common.pytorch_not_installed_status")
            } else {
                t!("cleaning.tools.watermark.run_hint")
            };
            if ui
                .add_enabled(can_run, egui::Button::new(mode.run_button_label()))
                .on_hover_text(run_hint)
                // `on_hover_text` is enabled-only, so the reason the button is
                // greyed out needs the disabled variant too.
                .on_disabled_hover_text(run_hint)
                .clicked()
            {
                // A run may download code or weights; re-check the catalog after it.
                *self.want_status = true;
                self.session
                    .start_run(editor, self.settings, self.progress, mode);
            }
            if ui
                .add_enabled(
                    can_undo,
                    egui::Button::new(t!("cleaning.mask_editor.revert_button")),
                )
                .clicked()
            {
                self.session.undo_last_run(editor);
            }
            if running {
                ui.spinner();
                if ui
                    .button(t!("cleaning.mask_editor.cancel_processing_button"))
                    .on_hover_text(t!("cleaning.mask_editor.cancel_processing_tooltip"))
                    .clicked()
                {
                    self.session.run_rx = None;
                    editor.status =
                        Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
                }
            }
        });
    }
}
