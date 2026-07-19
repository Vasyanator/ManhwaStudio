/*
File: panel/create_main_text.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
the main text-parameter UI. The "Параметры" sub-tab is grouped into collapsible
sections (font / glyph metrics / layout & alignment / shape & smoothing /
typeface style / text processing) drawn by `collapsing_param_section`, followed
by the unchanged advanced-params collapsing header.

Main responsibilities:
- draw the main text params container and its six collapsible sections;
- draw inline per-selection offset controls and alignment controls;
- report how many characters the current inline selection covers.

Key functions:
- collapsing_param_section() (free fn): one collapsible section with a strong
  title, an optional weak right-aligned summary, and a body; state persists per
  (id_salt, preview_enabled).
- draw_main_text_params(): builds the section list and wires the closures.
- draw_font_section / draw_metrics_section / draw_layout_alignment_section /
  draw_shape_render_section / draw_weight_section / draw_text_processing_section:
  the six section bodies (control code moved verbatim from the former
  left/right columns).

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so `panel.rs`
and sibling submodules can call them. `use super::*;` pulls in the parent
module's types and imports. Both call sites pass `stacked_columns = true`; the
non-stacked ("wide") branch is DEAD CODE kept only so the file compiles.
*/

use super::*;

/// Uniform vertical gap after each parameter section, so open and collapsed
/// sections keep the same rhythm.
const PARAM_SECTION_GAP_PX: f32 = 3.0;

/// Decides the font index the user actually PICKED in the font combo this frame,
/// if any — the edge that is allowed to write an inline span's font label.
///
/// - `popup_pick`: the index a popup option click selected (from
///   `draw_font_combo_option`). A click always counts as a pick, even on the
///   already-highlighted row (the user explicitly re-affirmed that font).
/// - `wheel`: `(before, after)` font indices around any applied wheel steps. A
///   wheel event counts only when it actually moved the index.
///
/// Returns `None` when nothing changed this frame, so inline-selection writeback
/// stays strictly edge-triggered: merely resolving/clamping `font_idx` per frame
/// never counts as a pick.
pub(super) fn font_combo_user_pick(
    popup_pick: Option<usize>,
    wheel: Option<(usize, usize)>,
) -> Option<usize> {
    if let Some(idx) = popup_pick {
        return Some(idx);
    }
    match wheel {
        Some((before, after)) if before != after => Some(after),
        _ => None,
    }
}

/// Draws a collapsible parameter section styled as a "header bar + left guide
/// rule".
///
/// The header row (toggle triangle + strong `title` + optional right-aligned
/// weak `summary`) sits on a faint, full-width background bar; the body is
/// drawn indented (`.body`) with a thin, faint vertical guide line down its
/// left edge to signal "these controls belong to the section above". Both the
/// bar and the guide use theme-derived colors (`Visuals::faint_bg_color` and
/// `Visuals::weak_text_color`), so the look is correct in the standard dark
/// theme and hard-codes no literal colors.
///
/// Composition (verified against `egui-0.35.0/src/containers/collapsing_header.rs`):
/// `HeaderResponse` borrows the same `ui` and its `.body(..)` consumes that
/// borrow, so the bar cannot be a `Frame` wrapped around `show_header`. Instead
/// a background shape slot is reserved BEFORE the header
/// (`painter().add(Shape::Noop)`) and filled AFTER, once the header row rect is
/// known, via `painter().set(..)` — so the bar paints behind the already-drawn
/// header. egui's built-in indent vline is suppressed for the body so it never
/// doubles with the guide we paint.
///
/// The open/closed state persists per `(id_salt, preview_enabled)` via
/// `egui::Id::new((id_salt, preview_enabled))`, so the create and edit panels
/// are independent and the state survives a UI-language switch (the id is
/// language independent — see `egui-docs/05-ids-and-i18n.md`).
///
/// `id_salt` is a persistence key (an i18n exclusion), not a caption; the
/// visible `title`/`summary` are already-localized strings supplied by the
/// caller. `add_body` paints the section contents when it is open.
pub(super) fn collapsing_param_section(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    preview_enabled: bool,
    title: &str,
    default_open: bool,
    summary: Option<&str>,
    add_body: impl FnOnce(&mut egui::Ui),
) {
    let id = egui::Id::new((id_salt, preview_enabled));

    // Full-width horizontal extent for the header bar: the section spans the
    // panel width even though the header ROW only sizes to its own content.
    let bar_x_range = ui.max_rect().x_range();
    // Reserve a slot for the bar BEFORE the header so it can be filled in behind
    // the toggle/title/summary once the header row rect is known.
    let bar_idx = ui.painter().add(egui::Shape::Noop);

    // We paint our own guide line; suppress egui's built-in indent vline for
    // this section (restored right after) so the two never double up.
    let prev_indent_vline = ui.visuals().indent_has_left_vline;
    ui.visuals_mut().indent_has_left_vline = false;

    let (_toggle, header_inner, body_inner) =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, default_open)
            .show_header(ui, |ui| {
                ui.label(egui::RichText::new(title).strong());
                if let Some(summary) = summary {
                    // Right-aligned, weak (faint) summary of the section's
                    // current state; skipped when there is no compact summary.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.weak(summary);
                    });
                }
            })
            // `body` (indented) so the contents sit visually under the header.
            .body(add_body);

    ui.visuals_mut().indent_has_left_vline = prev_indent_vline;

    // Faint full-width header bar behind the header row, with a little vertical
    // padding so the bar has some height around the text.
    let header_rect = header_inner.response.rect;
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(bar_x_range.min, header_rect.top() - 2.0),
        egui::pos2(bar_x_range.max, header_rect.bottom() + 2.0),
    );
    ui.painter().set(
        bar_idx,
        egui::Shape::rect_filled(bar_rect, 3.0, ui.visuals().faint_bg_color),
    );

    // Thin, faint vertical guide line along the left of the indented body
    // (present only while the section is open / animating).
    if let Some(body) = body_inner {
        let body_rect = body.response.rect;
        let indent = ui.spacing().indent;
        let guide_x = body_rect.left() - 0.5 * indent;
        let guide_stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color());
        ui.painter().vline(guide_x, body_rect.y_range(), guide_stroke);
    }

    ui.add_space(PARAM_SECTION_GAP_PX);
}

impl TypingCreatePanelState {

    pub(super) fn draw_main_text_params(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
        font_memory_enabled: bool,
        font_missing: bool,
    ) -> bool {
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        let inline_selection = if self.preview_enabled {
            None
        } else {
            self.inline_selection_context()
        };
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            // Precompute the per-section header summaries as a frame-start snapshot
            // of `self`. They are read-only borrows that end before the section
            // bodies mutate `self`; immediate-mode redraw catches up next frame.
            let preview_enabled = self.preview_enabled;
            let font_label = self
                .fonts
                .get(self.selected_font_idx)
                .map(|font| self.font_display_label(font))
                .unwrap_or_default();
            let font_summary = format!("{} · {}px", font_label, self.font_size_px.round() as i32);
            let layout_summary = match self.text_layout_mode {
                TextLayoutMode::Normal => t!("typing.advanced.layout_kind_standard"),
                TextLayoutMode::Formula => t!("typing.advanced.layout_kind_formula"),
                TextLayoutMode::Shape => t!("typing.advanced.layout_kind_shape"),
                TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines => {
                    t!("typing.advanced.layout_kind_vector_lines")
                }
            };
            let shape_label = match self.text_shape {
                TextShape::Free => t!("typing.params.shape_free_option"),
                TextShape::Rectangle => "[  ]",
                TextShape::Oval => "(  )",
                TextShape::Hexagon => "<  >",
                TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
            };
            let shape_summary = format!("{} · {}", shape_label, anti_aliasing_label(self.anti_aliasing));
            let enabled_count = usize::from(self.hanging_punctuation)
                + usize::from(self.trim_extra_spaces)
                + usize::from(self.new_line_after_sentence)
                + usize::from(self.uppercase_text)
                + usize::from(self.enable_inline_style_tags);
            let text_processing_summary = tf!("typing.section.enabled_count", count = enabled_count);

            if stacked_columns {
                collapsing_param_section(
                    ui,
                    "typing.section.font",
                    preview_enabled,
                    t!("typing.params.font_label"),
                    true,
                    Some(font_summary.as_str()),
                    |ui| {
                        self.draw_font_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_memory_enabled,
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    "typing.section.metrics",
                    preview_enabled,
                    t!("typing.section.metrics"),
                    true,
                    None,
                    |ui| {
                        self.draw_metrics_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    "typing.section.layout",
                    preview_enabled,
                    t!("typing.section.layout"),
                    true,
                    Some(layout_summary),
                    |ui| {
                        self.draw_layout_alignment_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    "typing.section.shape",
                    preview_enabled,
                    t!("typing.section.shape"),
                    false,
                    Some(shape_summary.as_str()),
                    |ui| {
                        self.draw_shape_render_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    "typing.section.weight",
                    preview_enabled,
                    t!("typing.section.weight"),
                    false,
                    None,
                    |ui| {
                        self.draw_weight_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );
                collapsing_param_section(
                    ui,
                    "typing.section.text_processing",
                    preview_enabled,
                    t!("typing.section.text_processing"),
                    false,
                    Some(text_processing_summary.as_str()),
                    |ui| {
                        self.draw_text_processing_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            inline_style.as_mut(),
                            font_missing,
                        );
                    },
                );

                // The advanced-params collapsing header keeps its original gating:
                // disabled while a font is missing (blocks re-render) and while an
                // inline selection is active. Its own contents are unchanged.
                ui.add_enabled_ui(!font_missing, |ui| {
                    ui.add_enabled_ui(!selection_mode, |ui| {
                        self.draw_advanced_text_params_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            "typing_advanced_text_params_right_column",
                        );
                    });
                });
            } else {
                // DEAD non-stacked ("wide") path: both call sites pass
                // `stacked_columns = true`, so this branch is never reached at
                // runtime. It is kept behavior-neutral and compiling by drawing the
                // same sections FLAT (no collapsibles) in order.
                self.draw_font_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_memory_enabled,
                    font_missing,
                );
                self.draw_metrics_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_layout_alignment_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_shape_render_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_weight_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                self.draw_text_processing_section(
                    ui,
                    &mut changed,
                    &mut block_hscroll_by_hovered_param,
                    inline_style.as_mut(),
                    font_missing,
                );
                ui.add_enabled_ui(!font_missing, |ui| {
                    ui.add_enabled_ui(!selection_mode, |ui| {
                        self.draw_advanced_text_params_section(
                            ui,
                            &mut changed,
                            &mut block_hscroll_by_hovered_param,
                            "typing_advanced_text_params_right_column",
                        );
                    });
                });
            }

            // Extra bottom padding so the horizontal scrollbar doesn't overlap the last checkbox text.
            ui.add_space(ui.spacing().scroll.allocated_width() + 4.0);
        });

        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        if let (Some(selection), Some(style)) = (inline_selection, inline_style) {
            changed |= self.apply_inline_style_to_selection(selection, style);
        }
        changed
    }

    /// Font section (default open): font-group / font / face selectors, the
    /// missing-font hint, and the color + size controls. The group/font/hint stay
    /// enabled even when a font is missing so the user can pick a replacement; the
    /// face selector is gated on `!selection_mode`; color + size are gated on
    /// `!font_missing`. Control code moved verbatim from the former panel body and
    /// left column.
    pub(super) fn draw_font_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
        font_memory_enabled: bool,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        // Комбобокс группы шрифтов показывается на обеих панелях (создание и
        // редактирование); выбор синхронизируется между ними через
        // `pending_font_group_request` (см. обработку во внешнем цикле).
        {
            let mut selected_group_idx = self
                .selected_font_group
                .as_ref()
                .and_then(|selected| {
                    self.font_groups.iter().position(|group| group == selected)
                })
                .map_or(0usize, |idx| idx + 1);
            let group_count = self.font_groups.len() + 1;
            let selected_group_text =
                self.selected_font_group.as_deref().unwrap_or(t!("typing.params.font_group_all"));
            // Same remembered-popup-size trap as the font combo below: salt with the
            // group list so adding/removing groups re-measures the popup height.
            let group_combo = WheelComboBox::from_label(t!("typing.create.font_group_combo_id")).id_salt(("typing.create.font_group_combo_id", &self.font_groups))
                .selected_text(selected_group_text)
                .show_ui_with_wheel(ui, |ui| {
                    ui.selectable_value(&mut selected_group_idx, 0usize, t!("typing.params.font_group_all"));
                    for (idx, group_name) in self.font_groups.iter().enumerate() {
                        ui.selectable_value(&mut selected_group_idx, idx + 1, group_name);
                    }
                });
            mark_hscroll_block_on_hover(
                block_hscroll_by_hovered_param,
                &group_combo.inner.response,
            );
            if let Some(steps) = group_combo.wheel_steps {
                cycle_wrapped_index(&mut selected_group_idx, group_count, steps);
            }
            let previous_group = self.selected_font_group.clone();
            self.selected_font_group = if selected_group_idx == 0 {
                None
            } else {
                self.font_groups.get(selected_group_idx - 1).cloned()
            };
            if self.selected_font_group != previous_group {
                self.ensure_selected_font_in_group();
                self.pending_font_group_request = Some(self.selected_font_group.clone());
                *changed = true;
            }
        }

        let prev_font_idx = self.selected_font_idx;
        let filtered_font_indices = self.filtered_font_indices();
        let selected_font_text: String = if font_missing {
            // Шрифт оверлея не найден: показываем его имя, чтобы было понятно,
            // какой именно шрифт отсутствует и какой надо заменить.
            self.missing_font
                .as_ref()
                .map(|name| tf!("typing.params.font_not_found_option", name = name))
                .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
        } else {
            inline_style
                .as_deref()
                .and_then(|style| style.font_label.as_deref())
                .map(|label| {
                    // DISPLAY ONLY: resolve the raw render label to its display label (a user
                    // rename override) when a font matches, so the CLOSED combo shows the same
                    // name as the popup rows. The span style's render key is never touched.
                    self.find_font_idx_by_label_preferring_indices(
                        Some(label),
                        &filtered_font_indices,
                    )
                    .and_then(|idx| self.fonts.get(idx))
                    .map(|font| self.font_display_label(font))
                    .unwrap_or_else(|| label.to_string())
                })
                .or_else(|| {
                    self.fonts
                        .get(self.selected_font_idx)
                        .map(|font| self.font_display_label(font))
                })
                .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
        };
        // Resolve the selection's/overlay's current font from its label. When a
        // group is active this PREFERS the in-group copy over a same-named font
        // outside the group, so an ambiguous label (e.g. an imported system font
        // colliding with a group member) does not silently resolve to the wrong
        // entry.
        let mut font_idx = inline_style
            .as_deref()
            .and_then(|style| {
                self.find_font_idx_by_label_preferring_indices(
                    style.font_label.as_deref(),
                    &filtered_font_indices,
                )
            })
            .unwrap_or(self.selected_font_idx);
        // DISPLAY-ONLY clamp: if the resolved font is outside the active group,
        // move the combo's highlight to the first visible entry so a valid row is
        // shown as selected. In inline-selection mode this clamped value is NEVER
        // written back into the span style (see the edge-triggered writeback
        // below) — otherwise merely selecting text would bounce the label to a
        // different font and re-insert a `<font>` tag every frame.
        if !filtered_font_indices.contains(&font_idx)
            && let Some(first_filtered_idx) = filtered_font_indices.first().copied()
        {
            font_idx = first_filtered_idx;
        }
        // A genuine user font pick THIS frame: a popup option click (captured in
        // `popup_pick`) or a wheel step that actually moved the index. Only such an
        // edge may mutate the span's font label in inline-selection mode; the
        // per-frame resolved/clamped `font_idx` alone must not.
        let mut popup_pick: Option<usize> = None;
        // The popup's Area remembers the previous content size and hands it back as
        // the next open's max_rect, while the inner ScrollArea clamps its height to
        // that available space (egui-0.35.0 area.rs:610,666 + scroll_area.rs:765).
        // After a small font group the popup would therefore stay ~3 rows tall
        // forever, even back on "all fonts". Salting the id with the filtered list
        // discards the remembered size whenever the popup content changes, so the
        // popup re-measures to its natural height (min(content, combo_height)).
        let font_combo = WheelComboBox::from_label(t!("typing.create.font_combo_id")).id_salt(("typing.create.font_combo_id", &filtered_font_indices))
            .selected_text(selected_font_text)
            .show_ui_with_wheel(ui, |ui| {
                for idx in filtered_font_indices.iter().copied() {
                    let (label, path, face_index, coverage) = {
                        let font = &self.fonts[idx];
                        (
                            self.font_display_label(font),
                            font.path.clone(),
                            font.faces.first().map(|face| face.face_index).unwrap_or(0),
                            font.coverage.clone(),
                        )
                    };
                    let selected = font_idx == idx;
                    if self.draw_font_combo_option(
                        ui,
                        &label,
                        path.as_path(),
                        face_index,
                        selected,
                        &coverage,
                    ) {
                        font_idx = idx;
                        popup_pick = Some(idx);
                    }
                }
            });
        mark_hscroll_block_on_hover(
            block_hscroll_by_hovered_param,
            &font_combo.inner.response,
        );
        // Apply any wheel steps to `font_idx` (used by the non-selection branch),
        // recording the before/after so the edge detector can tell a real move
        // from a no-op wheel event.
        let wheel = font_combo.wheel_steps.map(|steps| {
            let before = font_idx;
            cycle_wrapped_index_in_values(&mut font_idx, &filtered_font_indices, steps);
            (before, font_idx)
        });
        let user_picked_font_idx = font_combo_user_pick(popup_pick, wheel);
        if let Some(style) = inline_style.as_mut() {
            // Edge-triggered writeback (mirrors the non-selection branch's
            // `font_idx != prev_font_idx` guard): only a real pick this frame
            // writes the span font label, so selecting text can never insert a
            // `<font>` tag on its own.
            if let Some(picked) = user_picked_font_idx
                && let Some(label) = self.font_identity_name_by_idx(picked)
            {
                style.font_label = Some(label);
            }
        } else {
            self.selected_font_idx = font_idx;
            if self.selected_font_idx != prev_font_idx {
                // Любой выбор из списка — это доступный шрифт, поэтому снимаем
                // блокировку рендера по ненайденному шрифту.
                self.missing_font = None;
                if font_memory_enabled {
                    *changed |= self.handle_create_font_selection_change(prev_font_idx);
                } else {
                    self.selected_face_idx = 0;
                    *changed = true;
                }
            }
        }

        if font_missing {
            ui.colored_label(
                Color32::from_rgb(240, 200, 60),
                t!("typing.params.pick_available_font_hint"),
            );
        }

        ui.add_enabled_ui(!selection_mode, |ui| {
            let prev_face_idx = self.selected_face_idx;
            let selected_face_text = self
                .fonts
                .get(self.selected_font_idx)
                .and_then(|font| font.faces.get(self.selected_face_idx))
                .map(|face| face.label.as_str())
                .unwrap_or("<face>");
            let face_count = self
                .fonts
                .get(self.selected_font_idx)
                .map(|font| font.faces.len())
                .unwrap_or(0);
            let mut face_idx = self.selected_face_idx;
            let face_combo = WheelComboBox::from_label("Face")
                .selected_text(selected_face_text)
                .show_ui_with_wheel(ui, |ui| {
                    if let Some(font) = self.fonts.get(self.selected_font_idx) {
                        for (idx, face) in font.faces.iter().enumerate() {
                            ui.selectable_value(&mut face_idx, idx, &face.label);
                        }
                    }
                });
            mark_hscroll_block_on_hover(
                block_hscroll_by_hovered_param,
                &face_combo.inner.response,
            );
            if let Some(steps) = face_combo.wheel_steps {
                cycle_wrapped_index(&mut face_idx, face_count, steps);
            }
            self.selected_face_idx = face_idx;
            if self.selected_face_idx != prev_face_idx {
                *changed = true;
            }
        });

        // Остальные параметры влияют на рендер: при ненайденном шрифте они
        // блокируются, доступным остаётся только выбор шрифта выше.
        ui.add_enabled_ui(!font_missing, |ui| {
            if let Some(style) = inline_style.as_mut() {
                let mut text_color = style.text_color.unwrap_or(self.text_color);
                let color_resp = self.text_color_selector.draw(ui, &mut text_color);
                *changed |= color_resp.changed;
                style.text_color = Some(text_color);
                let mut font_size_px = style
                    .font_size_px
                    .unwrap_or(self.font_size_px)
                    .clamp(1.0, 256.0);
                let font_size_resp = ui.add(
                    WheelSlider::new(&mut font_size_px, 1.0..=256.0)
                        .text(t!("typing.params.size_px_label"))
                        .wheel_step(1.0),
                );
                *changed |= font_size_resp.changed();
                style.font_size_px = Some(font_size_px);
            } else {
                let color_resp = self.text_color_selector.draw(ui, &mut self.text_color);
                *changed |= color_resp.changed;
                let font_size_resp = ui.add(
                    WheelSlider::new(&mut self.font_size_px, 1.0..=256.0)
                        .text(t!("typing.params.size_px_label"))
                        .wheel_step(1.0),
                );
                *changed |= font_size_resp.changed();
            }
        });
    }

    /// Glyph-metrics section (default open, gated on `!font_missing`): line
    /// spacing, kerning mode + value, glyph height/width, and (in an inline
    /// selection) the per-selection offset controls. Moved verbatim from the
    /// former left column.
    pub(super) fn draw_metrics_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            let base_font_size_px = self.font_size_px.max(1.0);
            if let Some(style) = inline_style.as_mut() {
                let inline_font_size_px = style.font_size_px.unwrap_or(base_font_size_px).max(1.0);
                let mut line_spacing = style.line_spacing.unwrap_or(self.line_spacing);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.line_spacing_label"),
                    &mut line_spacing,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::LINE_SPACING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.line_spacing = Some(line_spacing);

                ui.horizontal(|ui| {
                    ui.label(t!("typing.params.kerning_label"));
                    // Read-only indicator of the global kerning mode (kerning is not a
                    // per-span inline override). Optical is not offered as a choice.
                    ui.add_enabled(
                        false,
                        egui::Button::new(t!("typing.params.kerning_metric"))
                            .selected(self.kerning_mode == KerningMode::Fixed),
                    );
                    ui.add_enabled(
                        false,
                        egui::Button::new(t!("typing.params.kerning_auto"))
                            .selected(self.kerning_mode == KerningMode::Auto),
                    );
                });
                let mut kerning = style.kerning.unwrap_or(self.kerning);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.kerning_label"),
                    &mut kerning,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::KERNING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.kerning = Some(kerning);

                let mut stretching = style
                    .glyph_stretching
                    .unwrap_or([self.glyph_width, self.glyph_height]);
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_height_label"),
                    &mut stretching[1],
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_HEIGHT),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_width_label"),
                    &mut stretching[0],
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: inline_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_WIDTH),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
                style.glyph_stretching = Some(stretching);
            } else {
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.line_spacing_label"),
                    &mut self.line_spacing,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::LINE_SPACING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                ui.horizontal(|ui| {
                    ui.label(t!("typing.params.kerning_label"));
                    // Optical is implemented but intentionally not offered here; only
                    // Fixed ("Метрический") and Auto ("Авто") are user-selectable.
                    *changed |= ui
                        .selectable_value(&mut self.kerning_mode, KerningMode::Fixed, t!("typing.params.kerning_metric"))
                        .changed();
                    *changed |= ui
                        .selectable_value(&mut self.kerning_mode, KerningMode::Auto, t!("typing.params.kerning_auto"))
                        .changed();
                });

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.kerning_label"),
                    &mut self.kerning,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 2.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::KERNING),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_height_label"),
                    &mut self.glyph_height,
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_HEIGHT),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                px_or_percent_param_row(
                    ui,
                    t!("typing.params.char_width_label"),
                    &mut self.glyph_width,
                    PxOrPercentRowCfg {
                        range: 1.0..=300.0,
                        wheel_step: 5.0,
                        font_size_px: base_font_size_px,
                        help: Some(ms_gifs::typing::CHAR_WIDTH),
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );
            }

            if selection_mode {
                self.draw_inline_offset_controls(
                    ui,
                    changed,
                    block_hscroll_by_hovered_param,
                    inline_style,
                );
            }
        });
    }

    /// Layout & alignment section (default open, gated on `!font_missing`): the
    /// global alignment controls, global rotation, and — for line-based layouts —
    /// line placement and the placement-reference combo (all gated on
    /// `!selection_mode`), plus the per-selection alignment controls when an
    /// inline selection is active. Moved verbatim from the former right column.
    pub(super) fn draw_layout_alignment_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                Self::draw_alignment_controls(
                    ui,
                    &mut self.align,
                    changed,
                    block_hscroll_by_hovered_param,
                );

                // Глобальный поворот всего блока: применяется к векторным контурам
                // глифов ДО растеризации, поэтому получается чётче, чем поворот уже
                // готового растра оверлея.
                ui.horizontal(|ui| {
                    // Deliberately two tooltips: the slider keeps its existing
                    // text tooltip, the "?" icon plays the animated hint.
                    let rotation_resp = ui
                        .add(
                            WheelSlider::new(&mut self.global_rotation_deg, -180.0..=180.0)
                                .text(t!("typing.params.global_rotation_label"))
                                .wheel_step(1.0),
                        )
                        .on_hover_text(t!("typing.params.global_rotation_tooltip"));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &rotation_resp);
                    *changed |= rotation_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &rotation_resp) {
                        *changed |=
                            apply_wheel_step_f32(&mut self.global_rotation_deg, steps, 1.0, -180.0, 180.0);
                    }
                    crate::widgets::HelpHint::animated(ms_gifs::typing::GLOBAL_ROTATION).show(ui);
                });

                // Размещение по линии: перпендикулярный сдвиг глифов относительно
                // линии/пути. Показывается только для линейных раскладок (формула и
                // векторные линии); для остальных режимов параметр скрыт и игнорируется
                // рендером.
                if matches!(
                    self.text_layout_mode,
                    TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines
                ) {
                    ui.horizontal(|ui| {
                        let placement_resp = ui
                            .add(
                                WheelSlider::new(&mut self.line_placement_percent, -100.0..=100.0)
                                    .text(t!("typing.params.line_placement_label"))
                                    .wheel_step(5.0),
                            )
                            .on_hover_text(
                                t!("typing.params.line_placement_tooltip"),
                            );
                        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &placement_resp);
                        *changed |= placement_resp.changed();
                        if let Some(steps) = wheel_steps_if_hovered(ui, &placement_resp) {
                            *changed |= apply_wheel_step_f32(
                                &mut self.line_placement_percent,
                                steps,
                                5.0,
                                -100.0,
                                100.0,
                            );
                        }

                        if ui.button("⬇").on_hover_text(t!("typing.params.line_placement_bottom")).clicked() {
                            self.line_placement_percent = -100.0;
                            *changed = true;
                        }
                        if ui.button("⬍").on_hover_text(t!("typing.params.line_placement_center")).clicked() {
                            self.line_placement_percent = 0.0;
                            *changed = true;
                        }
                        if ui.button("⬆").on_hover_text(t!("typing.params.line_placement_top")).clicked() {
                            self.line_placement_percent = 100.0;
                            *changed = true;
                        }
                    });
                }

                // Опора размещения: к чему привязывается «Размещение по линии» —
                // к общей строке (все символы на единой базовой линии, ровная изогнутая
                // строка) или к фактической высоте каждого символа (легаси, символы
                // «прыгают»). Только для векторных линий; формула этот режим не использует.
                if self.text_layout_mode == TextLayoutMode::CustomVectorLines {
                    let prev_reference = self.line_placement_reference;
                    let reference_combo = WheelComboBox::from_label(
                        t!("typing.params.line_placement_reference_label"),
                    )
                    .id_salt("typing.params.line_placement_reference")
                    .selected_text(match self.line_placement_reference {
                        LinePlacementReference::LineBox => {
                            t!("typing.params.line_placement_reference_line")
                        }
                        LinePlacementReference::GlyphHeight => {
                            t!("typing.params.line_placement_reference_glyph")
                        }
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.line_placement_reference,
                            LinePlacementReference::LineBox,
                            t!("typing.params.line_placement_reference_line"),
                        );
                        ui.selectable_value(
                            &mut self.line_placement_reference,
                            LinePlacementReference::GlyphHeight,
                            t!("typing.params.line_placement_reference_glyph"),
                        );
                    });
                    let reference_resp = reference_combo
                        .inner
                        .response
                        .on_hover_text(t!("typing.params.line_placement_reference_tooltip"));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &reference_resp);
                    // A wheel notch over the closed combo toggles between the two modes.
                    if let Some(steps) = reference_combo.wheel_steps
                        && steps != 0
                    {
                        self.line_placement_reference = match self.line_placement_reference {
                            LinePlacementReference::LineBox => LinePlacementReference::GlyphHeight,
                            LinePlacementReference::GlyphHeight => LinePlacementReference::LineBox,
                        };
                    }
                    if self.line_placement_reference != prev_reference {
                        *changed = true;
                    }
                }
            });

            // Per-selection alignment (inline style) — enabled while a selection is
            // active; moved here from the former right column's inline block.
            if let Some(style) = inline_style {
                let mut align = style.align.unwrap_or(self.align);
                Self::draw_alignment_controls(ui, &mut align, changed, block_hscroll_by_hovered_param);
                style.align = Some(align);
            }
        });
    }

    /// Shape & smoothing section (default collapsed, gated on `!font_missing` then
    /// `!selection_mode`): the shape / wrap / anti-aliasing combos, the
    /// moderate-herringbone checkbox, and the shape-specific min-width / variant
    /// sliders. Moved verbatim from the former right column.
    pub(super) fn draw_shape_render_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                let prev_shape = self.text_shape;
                let shape_combo = WheelComboBox::from_label(t!("typing.create.shape_combo_id")).id_salt("typing.create.shape_combo_id")
                    .selected_text(match self.text_shape {
                        TextShape::Free => t!("typing.params.shape_free_option"),
                        TextShape::Rectangle => "[  ]",
                        TextShape::Oval => "(  )",
                        TextShape::Hexagon => "<  >",
                        TextShape::SoftPeak => t!("typing.params.shape_soft_option"),
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(&mut self.text_shape, TextShape::Free, t!("typing.params.shape_free_option"));
                        ui.selectable_value(&mut self.text_shape, TextShape::Rectangle, "[  ]");
                        ui.selectable_value(&mut self.text_shape, TextShape::Oval, "(  )");
                        ui.selectable_value(&mut self.text_shape, TextShape::Hexagon, "<  >");
                        ui.selectable_value(&mut self.text_shape, TextShape::SoftPeak, t!("typing.params.shape_soft_option"));
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &shape_combo.inner.response,
                );
                if let Some(steps) = shape_combo.wheel_steps {
                    *changed |= cycle_text_shape(&mut self.text_shape, steps);
                }
                if self.text_shape != prev_shape {
                    *changed = true;
                }

                let prev_wrap_mode = self.text_wrap_mode;
                let wrap_combo = WheelComboBox::from_label(t!("typing.create.wrap_combo_id")).id_salt("typing.create.wrap_combo_id")
                    .selected_text(text_wrap_mode_label(self.text_wrap_mode))
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::None,
                            text_wrap_mode_label(TextWrapMode::None),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::WholeWords,
                            text_wrap_mode_label(TextWrapMode::WholeWords),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Minimal,
                            text_wrap_mode_label(TextWrapMode::Minimal),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Moderate,
                            text_wrap_mode_label(TextWrapMode::Moderate),
                        );
                        ui.selectable_value(
                            &mut self.text_wrap_mode,
                            TextWrapMode::Aggressive,
                            text_wrap_mode_label(TextWrapMode::Aggressive),
                        );
                    });
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &wrap_combo.inner.response);
                if let Some(steps) = wrap_combo.wheel_steps {
                    *changed |= cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
                }
                if self.text_wrap_mode != prev_wrap_mode {
                    self.sync_wrap_mode_constraints();
                    *changed = true;
                }

                let prev_anti_aliasing = self.anti_aliasing;
                // Horizontal row so the animated help icon sits after the
                // combo's right-hand label.
                let aa_combo = ui
                    .horizontal(|ui| {
                        let aa_combo = WheelComboBox::from_label(t!("typing.create.antialias_combo_id")).id_salt("typing.create.antialias_combo_id")
                            .selected_text(anti_aliasing_label(self.anti_aliasing))
                            .show_ui_with_wheel(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::None,
                                    anti_aliasing_label(AntiAliasingMode::None),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Sharp,
                                    anti_aliasing_label(AntiAliasingMode::Sharp),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Crisp,
                                    anti_aliasing_label(AntiAliasingMode::Crisp),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Strong,
                                    anti_aliasing_label(AntiAliasingMode::Strong),
                                );
                                ui.selectable_value(
                                    &mut self.anti_aliasing,
                                    AntiAliasingMode::Smooth,
                                    anti_aliasing_label(AntiAliasingMode::Smooth),
                                );
                            });
                        crate::widgets::HelpHint::animated(ms_gifs::typing::ANTI_ALIASING).show(ui);
                        aa_combo
                    })
                    .inner;
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &aa_combo.inner.response);
                if let Some(steps) = aa_combo.wheel_steps {
                    *changed |= cycle_anti_aliasing(&mut self.anti_aliasing, steps);
                }
                if self.anti_aliasing != prev_anti_aliasing {
                    *changed = true;
                }
                let moderate_trees_resp = ui.add_enabled(
                    self.moderate_trees_checkbox_enabled(),
                    egui::Checkbox::new(&mut self.allow_moderate_trees, t!("typing.params.allow_moderate_herringbone")),
                );
                *changed |= moderate_trees_resp.changed();

                if matches!(self.text_shape, TextShape::Oval | TextShape::Hexagon) {
                    let min_width_resp = ui.add(
                        WheelSlider::new(&mut self.shape_min_width_percent, 5.0..=100.0)
                            .text(t!("typing.params.min_width_percent_label")),
                    );
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &min_width_resp);
                    *changed |= min_width_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &min_width_resp) {
                        *changed |= apply_wheel_step_f32(
                            &mut self.shape_min_width_percent,
                            steps,
                            1.0,
                            5.0,
                            100.0,
                        );
                    }
                }
                if self.text_shape == TextShape::SoftPeak {
                    let variant_resp =
                        ui.add(WheelSlider::new(&mut self.shape_variant, 1..=9).text(t!("typing.params.shape_variant_label")));
                    mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &variant_resp);
                    *changed |= variant_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &variant_resp) {
                        *changed |= apply_wheel_step_u8(&mut self.shape_variant, steps, 1, 1, 9);
                    }
                }
            });
        });
    }

    /// Typeface-style section (default collapsed, gated on `!font_missing`): the
    /// faux bold/italic controls, plus the inline `no_break` checkbox in an inline
    /// selection. The per-selection alignment controls live in the layout section.
    /// Moved verbatim from the former right column's faux-style block.
    pub(super) fn draw_weight_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        ui.add_enabled_ui(!font_missing, |ui| {
            if let Some(style) = inline_style {
                let mut bold = style.bold;
                let mut italic = style.italic;
                let faux = style.faux_bold.unwrap_or_default();
                let mut thicken = faux.thicken_percent;
                let mut expand = faux.expand_percent;
                let mut sharp = faux.sharp_corners;
                let mut outward = faux.outward_only;
                let mut faux_bold = style.faux_bold.is_some();
                let mut slant = style.faux_italic_slant.unwrap_or(14.0);
                let mut faux_italic = style.faux_italic_slant.is_some();
                draw_faux_style_controls(
                    ui,
                    &mut bold,
                    &mut italic,
                    FauxStyleControlValues {
                        faux_bold: &mut faux_bold,
                        faux_bold_thicken_percent: &mut thicken,
                        faux_bold_expand_percent: &mut expand,
                        faux_bold_sharp_corners: &mut sharp,
                        faux_bold_outward_only: &mut outward,
                        faux_italic: &mut faux_italic,
                        faux_italic_slant_deg: &mut slant,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                    "typing_main_inline_faux",
                );
                style.bold = bold;
                style.italic = italic;
                style.faux_bold = (bold && faux_bold).then_some(FauxBoldParams {
                    thicken_percent: thicken,
                    expand_percent: expand,
                    sharp_corners: sharp,
                    outward_only: outward,
                });
                style.faux_italic_slant = (italic && faux_italic).then_some(slant);

                let mut no_break = style.no_break;
                let no_break_resp = ui.checkbox(&mut no_break, t!("typing.params.no_break"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &no_break_resp);
                *changed |= no_break_resp.changed();
                style.no_break = no_break;
            } else {
                draw_faux_style_controls(
                    ui,
                    &mut self.force_bold,
                    &mut self.force_italic,
                    FauxStyleControlValues {
                        faux_bold: &mut self.faux_bold,
                        faux_bold_thicken_percent: &mut self.faux_bold_thicken_percent,
                        faux_bold_expand_percent: &mut self.faux_bold_expand_percent,
                        faux_bold_sharp_corners: &mut self.faux_bold_sharp_corners,
                        faux_bold_outward_only: &mut self.faux_bold_outward_only,
                        faux_italic: &mut self.faux_italic,
                        faux_italic_slant_deg: &mut self.faux_italic_slant_deg,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                    "typing_main_faux",
                );
            }
        });
    }

    /// Text-processing section (default collapsed, gated on `!font_missing` then
    /// `!selection_mode`): the five processing checkboxes (hanging punctuation,
    /// strip extra spaces, newline after sentence, all-uppercase, enable inline
    /// tags). Moved verbatim from the former right column.
    pub(super) fn draw_text_processing_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
        font_missing: bool,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!font_missing, |ui| {
            ui.add_enabled_ui(!selection_mode, |ui| {
                // Horizontal row so the animated help icon sits after the checkbox label.
                let hanging_punct_resp = ui
                    .horizontal(|ui| {
                        let resp = ui
                            .checkbox(&mut self.hanging_punctuation, t!("typing.params.hanging_punctuation"));
                        crate::widgets::HelpHint::animated(ms_gifs::typing::HANGING_PUNCTUATION).show(ui);
                        resp
                    })
                    .inner;
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &hanging_punct_resp);
                *changed |= hanging_punct_resp.changed();
                let trim_spaces_resp =
                    ui.checkbox(&mut self.trim_extra_spaces, t!("typing.params.strip_extra_spaces"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &trim_spaces_resp);
                *changed |= trim_spaces_resp.changed();
                let sentence_nl_resp = ui.checkbox(
                    &mut self.new_line_after_sentence,
                    t!("typing.params.newline_after_sentence"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sentence_nl_resp);
                *changed |= sentence_nl_resp.changed();
                let uppercase_text_resp =
                    ui.checkbox(&mut self.uppercase_text, t!("typing.params.all_uppercase"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &uppercase_text_resp);
                *changed |= uppercase_text_resp.changed();
                let inline_tags_resp = ui.checkbox(
                    &mut self.enable_inline_style_tags,
                    t!("typing.params.parse_bi_tags"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inline_tags_resp);
                *changed |= inline_tags_resp.changed();
            });
        });
    }

    pub(super) fn draw_inline_offset_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let inline_font_size_px = inline_style
            .as_ref()
            .and_then(|style| style.font_size_px)
            .unwrap_or(self.font_size_px)
            .max(1.0);
        ui.add_enabled_ui(inline_style.is_some(), |ui| {
            let mut offset = inline_style
                .as_ref()
                .and_then(|style| style.glyph_offset)
                .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0]));
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_x_label"),
                &mut offset.global_x,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                    help: None,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                t!("typing.params.inline_offset_y_label"),
                &mut offset.global_y,
                PxOrPercentRowCfg {
                    range: -100.0..=100.0,
                    wheel_step: 1.0,
                    font_size_px: inline_font_size_px,
                    help: None,
                },
                changed,
                block_hscroll_by_hovered_param,
            );
            // Смещение по линии — линейная концепция: строка показывается только
            // для линейных раскладок (формула и кастомные векторные линии), как и
            // «Размещение по линии». В остальных режимах значение сохраняется, но
            // и сама строка, и её подпараметр скрыты.
            let line_based_layout = matches!(
                self.text_layout_mode,
                TextLayoutMode::Formula | TextLayoutMode::CustomVectorLines
            );
            if line_based_layout {
                px_or_percent_param_row(
                    ui,
                    t!("typing.params.inline_offset_along_line_label"),
                    &mut offset.line,
                    PxOrPercentRowCfg {
                        range: -300.0..=300.0,
                        wheel_step: 1.0,
                        font_size_px: inline_font_size_px,
                        help: None,
                    },
                    changed,
                    block_hscroll_by_hovered_param,
                );

                // «Сдвигать следующие символы» — подпараметр смещения по линии:
                // группируется под отступ-линией (как параметры faux bold под
                // чекбоксом) и появляется только при ненулевом смещении.
                if offset.line.value != 0.0 {
                    ui.indent(Id::new("typing_inline_shift_following"), |ui| {
                        *changed |= ui
                            .checkbox(
                                &mut offset.shift_following,
                                t!("typing.params.inline_shift_following"),
                            )
                            .changed();
                    });
                }
            }

            let group_enabled = inline_style
                .as_ref()
                .is_some_and(|_| self.selected_inline_char_count() > 1);
            ui.add_enabled_ui(group_enabled, |ui| {
                let group_resp = ui.add(
                    WheelSlider::new(&mut offset.group_rotation_deg, -180.0..=180.0)
                        .text(t!("typing.params.inline_group_rotation_label"))
                        .wheel_step(1.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &group_resp);
                *changed |= group_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &group_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut offset.group_rotation_deg,
                        steps,
                        1.0,
                        -180.0,
                        180.0,
                    );
                }
            });
            if !group_enabled {
                offset.group_rotation_deg = 0.0;
            }

            let glyph_resp = ui.add(
                WheelSlider::new(&mut offset.glyph_rotation_deg, -180.0..=180.0)
                    .text(t!("typing.params.inline_char_rotation_label"))
                    .wheel_step(1.0),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &glyph_resp);
            *changed |= glyph_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &glyph_resp) {
                *changed |=
                    apply_wheel_step_f32(&mut offset.glyph_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
            if let Some(style) = inline_style {
                style.glyph_offset = Some(offset);
            }
        });
    }

    pub(super) fn selected_inline_char_count(&self) -> usize {
        self.text_selection_char_range
            .as_ref()
            .map(|range| range.end.saturating_sub(range.start))
            .unwrap_or(0)
    }

    /// Управление выравниванием на ОДНОЙ строке: слайдер лево↔право (`-100..100`),
    /// быстрые кнопки (⬅ влево / ⬇ по центру / ➡ вправо) и зажимаемая кнопка-тоггл
    /// ⬌ (justify, «Растягивать по ширине блока»). Слайдер и стрелки отключаются при
    /// включённом justify; кнопка ⬌ остаётся активной, чтобы его можно было выключить.
    pub(super) fn draw_alignment_controls(
        ui: &mut egui::Ui,
        align: &mut HorizontalAlign,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let free_align = align.justify;
        ui.horizontal(|ui| {
            // Слайдер + стрелки отключаются при включённом justify.
            ui.add_enabled_ui(!free_align, |ui| {
                let mut bias_percent = (align.bias.clamp(-1.0, 1.0) * 100.0).round() as i32;
                let slider_resp = ui.add(
                    WheelSlider::new(&mut bias_percent, -100..=100)
                        .text(t!("typing.params.alignment_label"))
                        .wheel_step(5),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
                if slider_resp.changed() {
                    align.bias = bias_percent as f32 / 100.0;
                    *changed = true;
                }

                if ui.button("⬅").on_hover_text(t!("typing.params.align_left")).clicked() {
                    align.bias = -1.0;
                    *changed = true;
                }
                if ui.button("⬇").on_hover_text(t!("typing.params.align_center")).clicked() {
                    align.bias = 0.0;
                    *changed = true;
                }
                if ui.button("➡").on_hover_text(t!("typing.params.align_right")).clicked() {
                    align.bias = 1.0;
                    *changed = true;
                }
            });

            // Зажимаемая кнопка-тоггл justify — остаётся активной даже при включённом
            // justify, чтобы его можно было снять.
            if ui
                .add(egui::Button::new("⬌").selected(align.justify))
                .on_hover_text(t!("typing.params.justify_lines"))
                .clicked()
            {
                align.justify = !align.justify;
                *changed = true;
            }

            // Animated help for the whole alignment row. Kept OUTSIDE the
            // `add_enabled_ui(!free_align, ..)` scope above: a disabled icon
            // would never show its tooltip (`on_hover_ui` is enabled-only,
            // egui-0.35.0/src/response.rs:645), and the hint must stay
            // reachable while justify is on.
            crate::widgets::HelpHint::animated(ms_gifs::typing::ALIGNMENT).show(ui);
        });
    }

}
