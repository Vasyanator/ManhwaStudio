/*
FILE OVERVIEW: src/tabs/settings/canvas_ribbon.rs
Ribbon/canvas settings pane UI for the settings tab.

Main responsibilities:
- Render shared canvas ribbon settings previously edited inside `CanvasView`.
- Publish updated `SharedCanvasSettings` into shared models so other tabs pick them up.

Key types:
- `SettingsTabState`
- `SharedCanvasSettings`

Key functions:
- `SettingsTabState::draw_canvas_ribbon`

Notes:
- Persistence is coalesced by a background worker in `settings/mod.rs`.
*/

use super::{DraggedBubbleConditionNode, SettingsTabState};
use crate::bubble_status::{
    BubbleBorderKind, BubbleBorderStyle, BubbleStatusCondition, BubbleStatusField,
    BubbleStatusRule, default_bubble_status_rules, normalize_bubble_status_rules,
};
use crate::canvas::{AsideBubbleCompactMode, AsideBubbleSideMode, BubbleType, OnTopFocusMode};
use crate::project::ComicType;
use crate::widgets::{WheelComboBox as ComboBox, WheelSlider};
use egui::{
    Align2, Color32, FontId, Frame, Id, LayerId, Order, Pos2, Rect, RichText, ScrollArea, Sense,
    Stroke, Ui, vec2,
};

const RULE_CARD_HEIGHT_PX: f32 = 220.0;
const CONDITION_CARD_HEIGHT_PX: f32 = 112.0;
const RULES_LIST_MAX_HEIGHT_PX: f32 = 560.0;
const BUBBLE_STATUS_BLOCK_HEIGHT_PX: f32 = 760.0;
const INLINE_SLOT_MIN_WIDTH_PX: f32 = 124.0;
const CONDITION_BLOCK_MIN_WIDTH_PX: f32 = 260.0;

#[derive(Debug, Clone)]
struct PendingConditionDrop {
    target_rule_id: u64,
    target_path: Vec<usize>,
    source_rule_id: u64,
    source_path: Vec<usize>,
    payload: BubbleStatusCondition,
}

fn empty_group_slots() -> Vec<BubbleStatusCondition> {
    vec![BubbleStatusCondition::Empty, BubbleStatusCondition::Empty]
}

impl SettingsTabState {
    pub(super) fn draw_canvas_ribbon(&mut self, ui: &mut egui::Ui) {
        self.refresh_spellcheck_words_if_needed();
        let mut changed = false;
        ScrollArea::vertical()
            .id_salt("settings_canvas_ribbon_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.heading("Лента");
                ui.small("Эти настройки общие для вкладок «Перевод», «Клининг» и «Текст».");
                ui.add_space(8.0);

                let active_preset = ComicType::from_canvas_preset_fields(
                    &self.canvas_settings.aside_compact_mode,
                    self.canvas_settings.separate_pages,
                );
                let mut selected_preset = active_preset;
                ui.horizontal(|ui| {
                    ui.label("Пресет настроек");
                    ComboBox::from_id_salt("settings_canvas_ribbon_preset")
                        .selected_text(selected_preset.display_name())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected_preset,
                                ComicType::Pages,
                                ComicType::Pages.display_name(),
                            );
                            ui.selectable_value(
                                &mut selected_preset,
                                ComicType::Ribbon,
                                ComicType::Ribbon.display_name(),
                            );
                            ui.selectable_value(
                                &mut selected_preset,
                                ComicType::Custom,
                                ComicType::Custom.display_name(),
                            );
                        });
                });
                ui.small(
                    "При ручном изменении параметров пресета он автоматически переключится на «Свой».",
                );
                if selected_preset != active_preset
                    && let Some((aside_compact_mode, separate_pages)) =
                        selected_preset.canvas_preset()
                    {
                        self.canvas_settings.aside_compact_mode = aside_compact_mode.to_string();
                        self.canvas_settings.separate_pages = separate_pages;
                        changed = true;
                    }
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    ui.label("Стандартный тип пузырей во вкладке перевода").on_hover_text(
                        "Новые пузыри и стандартные пузыри создаются этого типа",
                    );
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.editable_bubble_type,
                            BubbleType::Aside.as_str().to_string(),
                            "Сбоку",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.editable_bubble_type,
                            BubbleType::OnTop.as_str().to_string(),
                            "Поверх",
                        )
                        .changed();
                });

                ui.horizontal(|ui| {
                    ui.label("Стандартный тип пузырей во вкладках клина и текста").on_hover_text(
                        "Стандартные пузыри в этих вкладках отображаются этим типом",
                    );
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.readonly_bubble_type,
                            BubbleType::Aside.as_str().to_string(),
                            "Сбоку",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.readonly_bubble_type,
                            BubbleType::OnTop.as_str().to_string(),
                            "Поверх",
                        )
                        .changed();
                });

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.auto_insert_last_character,
                        "Автоматически вставлять последнего персонажа",
                    )
                    .on_hover_text(
                        "При создании нового пузыря автоматически подставляет последнего использованного персонажа",
                    )
                    .changed();

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.spellcheck_original,
                        "Проверять орфографию в оригинале",
                    )
                    .changed();

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.spellcheck_translation,
                        "Проверять орфографию в переводе",
                    )
                    .changed();

                ui.collapsing("Кастомные слова для орфографии", |ui| {
                    ui.small("По одному слову на строку. Оба списка исключают слова из орфографии.");
                    ui.label("Общие исключения");
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut self.spellcheck_custom_words)
                                .desired_rows(6)
                                .desired_width(f32::INFINITY)
                                .hint_text("Например:\nWebtoon\nМанхва\nOCR"),
                        )
                        .changed();
                    ui.add_space(8.0);
                    ui.label("Исключения проекта");
                    changed |= ui
                        .add(
                            egui::TextEdit::multiline(&mut self.project_spellcheck_custom_words)
                                .desired_rows(6)
                                .desired_width(f32::INFINITY)
                                .hint_text("Например:\nМурим\nЧхонма\nЧонгук"),
                        )
                        .changed();
                });

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.scale_bubbles,
                        "Растягивать боковые пузыри",
                    )
                    .on_hover_text(concat!(
                        "Если включено, пузыри не будут выезжать за границы экрана, ",
                        "пока они между мин и макс шириной. \n",
                        "Если выключено, то пузыри всегда минимальной ширины"
                    ))
                    .changed();

                ui.label("Уменьшить боковые пузыри во вкладке перевода:");
                ui.horizontal_wrapped(|ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_compact_mode,
                            AsideBubbleCompactMode::None.as_str().to_string(),
                            "Нет",
                        ).on_hover_text("Весь интерфейс пузыря показывается всегда")
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_compact_mode,
                            AsideBubbleCompactMode::Moderate.as_str().to_string(),
                            "Умеренно",
                        ).on_hover_text(concat!(
                            "Пока пузырь не в фокусе, показывается только строка оригинала и перевода.\n",
                            "Если пузырь в фокусе, он раскрывается, появляются кнопки, номер и персонаж."
                        ))
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_compact_mode,
                            AsideBubbleCompactMode::Strong.as_str().to_string(),
                            "Сильно",
                        ).on_hover_text(concat!(
                            "Пока пузырь не в фокусе, показывается только строка перевода. Или оригинала, если перевод пустой.\n",
                            "Если пузырь в фокусе, он раскрывается, появляются обе строки, кнопки, номер и персонаж."
                        ))
                        .changed();
                });

                ui.label("Сторона боковых пузырей:");
                ui.horizontal_wrapped(|ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_side_mode,
                            AsideBubbleSideMode::Auto.as_str().to_string(),
                            "Авто",
                        )
                        .on_hover_text(
                            "Пузырь показывается слева или справа в зависимости от положения на странице",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_side_mode,
                            AsideBubbleSideMode::Left.as_str().to_string(),
                            "Слева",
                        )
                        .on_hover_text("Все боковые пузыри показываются слева от страницы")
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.aside_side_mode,
                            AsideBubbleSideMode::Right.as_str().to_string(),
                            "Справа",
                        )
                        .on_hover_text("Все боковые пузыри показываются справа от страницы")
                        .changed();
                });

                ui.label("Раскрытие пузырей типа \"Поверх\":");
                ui.horizontal_wrapped(|ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.on_top_focus_mode,
                            OnTopFocusMode::Around.as_str().to_string(),
                            "Вокруг",
                        )
                        .on_hover_text("При фокусе пузырь типа «Поверх» остаётся поверх страницы. \n Строка оригинала появляется сверху, а персонаж и кнопки снизу.")
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.canvas_settings.on_top_focus_mode,
                            OnTopFocusMode::Aside.as_str().to_string(),
                            "Сбоку",
                        )
                        .on_hover_text("При фокусе пузырь типа «Поверх» временно раскрывается как боковой")
                        .changed();
                });

                changed |= ui
                    .add(
                        WheelSlider::new(&mut self.canvas_settings.aside_scale_pct, 25..=300)
                            .text("Размер боковых пузырей (%)"),
                    )
                    .changed();

                changed |= ui
                    .add(
                        WheelSlider::new(&mut self.canvas_settings.bubble_min_width, 40.0..=5000.0)
                            .text("Мин. ширина боковых пузырей"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        WheelSlider::new(&mut self.canvas_settings.bubble_max_width, 40.0..=5000.0)
                            .text("Макс. ширина боковых пузырей"),
                    )
                    .changed();
                if self.canvas_settings.bubble_max_width < self.canvas_settings.bubble_min_width {
                    self.canvas_settings.bubble_max_width = self.canvas_settings.bubble_min_width;
                    changed = true;
                }

                ui.separator();

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.separate_pages,
                        "Разделять страницы",
                    )
                    .changed();
                changed |= ui
                    .add_enabled(
                        self.canvas_settings.separate_pages,
                        WheelSlider::new(&mut self.canvas_settings.page_spacing, 0.0..=5000.0)
                            .text("Межстраничный отступ"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        WheelSlider::new(&mut self.canvas_settings.edge_margin, 0.0..=5000.0)
                            .text("Отступ сверху/снизу"),
                    )
                    .changed();

                ui.separator();

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.tabs_autosync_enabled,
                        "Автосинхронизация между вкладками",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut self.canvas_settings.cache_pages, "Кэшировать страницы")
                    .changed();

                ui.separator();
                changed |= self.draw_bubble_status_rules_block(ui);
            });

        if changed {
            self.publish_canvas_settings();
        }
    }

    fn draw_bubble_status_rules_block(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;

        ui.allocate_ui(vec2(ui.available_width(), BUBBLE_STATUS_BLOCK_HEIGHT_PX), |ui| {
            Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_height(BUBBLE_STATUS_BLOCK_HEIGHT_PX);

                ui.heading("Статус пузырей");
                ui.small(
                    "Правила применяются сверху вниз. Первое совпавшее условие задаёт тип и цвет рамки пузыря.",
                );
                ui.add_space(6.0);

                changed |= ui
                    .checkbox(
                        &mut self.canvas_settings.show_bubble_status,
                        "Отображать статус пузырей",
                    )
                    .on_hover_text(
                        "Показывает рамку у editable aside/on_top пузырей по настраиваемым условиям",
                    )
                    .changed();

                ui.horizontal_wrapped(|ui| {
                    if ui.button("Сбросить на стандартный пресет").clicked() {
                        self.canvas_settings.bubble_status_rules = default_bubble_status_rules();
                        self.dragged_bubble_condition_node = None;
                        changed = true;
                    }
                });

                let dragged_node = &mut self.dragged_bubble_condition_node;

                let mut move_from = None;
                let mut move_to = None;
                let mut remove_idx = None;
                let mut pending_drop = None;
                let rules_len = self.canvas_settings.bubble_status_rules.len();
                ui.add_space(8.0);

                ScrollArea::vertical()
                    .id_salt("bubble_status_rules_list")
                    .auto_shrink([false, false])
                    .max_height(RULES_LIST_MAX_HEIGHT_PX)
                    .show(ui, |ui| {
                        for idx in 0..rules_len {
                            let rule_id = self.canvas_settings.bubble_status_rules[idx].id;
                            let can_move_down = idx + 1 < rules_len;
                            ui.push_id(rule_id, |ui| {
                                let rule = &mut self.canvas_settings.bubble_status_rules[idx];
                                Frame::group(ui.style()).show(ui, |ui| {
                                    ui.set_min_height(RULE_CARD_HEIGHT_PX);
                                    ui.horizontal_top(|ui| {
                                        ui.set_width((ui.available_width() - 210.0).max(360.0));
                                        changed |= draw_condition_card(
                                            ui,
                                            &mut rule.condition,
                                            rule_id,
                                            &[],
                                            true,
                                            dragged_node,
                                            &mut pending_drop,
                                        );

                                        ui.separator();

                                        ui.vertical(|ui| {
                                            ui.set_min_height(RULE_CARD_HEIGHT_PX);
                                            ui.strong(format!("Условие {}", idx + 1));
                                            ui.small(rule.condition.summary());
                                            ui.add_space(8.0);
                                            ui.label("Порядок");
                                            if ui
                                                .add_enabled(idx > 0, egui::Button::new("Вверх"))
                                                .clicked()
                                            {
                                                move_from = Some(idx);
                                                move_to = Some(idx - 1);
                                            }
                                            if ui
                                                .add_enabled(can_move_down, egui::Button::new("Вниз"))
                                                .clicked()
                                            {
                                                move_from = Some(idx);
                                                move_to = Some(idx + 1);
                                            }
                                            if ui.button("Удалить").clicked() {
                                                remove_idx = Some(idx);
                                            }

                                            ui.add_space(8.0);
                                            ui.label("Обводка");
                                            changed |= draw_border_kind_selector(
                                                ui,
                                                &format!("rule_{}_border_kind", rule.id),
                                                &mut rule.border.kind,
                                            );
                                            let mut color = rule.border.color32();
                                            if ui.color_edit_button_srgba(&mut color).changed() {
                                                rule.border.set_color32(color);
                                                changed = true;
                                            }
                                        });
                                    });
                                });
                            });
                            ui.add_space(6.0);
                        }
                    });

                if let (Some(from), Some(to)) = (move_from, move_to) {
                    self.canvas_settings.bubble_status_rules.swap(from, to);
                    changed = true;
                }

                if let Some(drop) = pending_drop.take() {
                    changed |= apply_condition_drop(&mut self.canvas_settings.bubble_status_rules, drop);
                    self.dragged_bubble_condition_node = None;
                }

                if let Some(idx) = remove_idx {
                    let removed_rule_id = self.canvas_settings.bubble_status_rules[idx].id;
                    self.canvas_settings.bubble_status_rules.remove(idx);
                    if self
                        .dragged_bubble_condition_node
                        .as_ref()
                        .is_some_and(|dragged| dragged.rule_id == removed_rule_id)
                    {
                        self.dragged_bubble_condition_node = None;
                    }
                    if self.canvas_settings.bubble_status_rules.is_empty() {
                        self.canvas_settings.bubble_status_rules = default_bubble_status_rules();
                    }
                    changed = true;
                }

                if changed {
                    normalize_bubble_status_rules(&mut self.canvas_settings.bubble_status_rules);
                }
            });
        });

        if ui.ctx().input(|i| i.pointer.any_released()) {
            self.dragged_bubble_condition_node = None;
        }

        draw_dragged_condition_preview(ui.ctx(), self.dragged_bubble_condition_node.as_ref());

        changed
    }
}

fn draw_border_kind_selector(ui: &mut Ui, id_source: &str, kind: &mut BubbleBorderKind) -> bool {
    let mut changed = false;
    ComboBox::from_id_salt(id_source)
        .selected_text(kind.label())
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(
                    kind,
                    BubbleBorderKind::Solid,
                    BubbleBorderKind::Solid.label(),
                )
                .changed();
            changed |= ui
                .selectable_value(
                    kind,
                    BubbleBorderKind::Dashed,
                    BubbleBorderKind::Dashed.label(),
                )
                .changed();
            changed |= ui
                .selectable_value(
                    kind,
                    BubbleBorderKind::Dotted,
                    BubbleBorderKind::Dotted.label(),
                )
                .changed();
            changed |= ui
                .selectable_value(kind, BubbleBorderKind::Wavy, BubbleBorderKind::Wavy.label())
                .changed();
        });
    changed
}

fn draw_condition_card(
    ui: &mut Ui,
    condition: &mut BubbleStatusCondition,
    rule_id: u64,
    path: &[usize],
    is_root: bool,
    dragged_node: &mut Option<DraggedBubbleConditionNode>,
    pending_drop: &mut Option<PendingConditionDrop>,
) -> bool {
    let mut changed = false;

    match &mut *condition {
        BubbleStatusCondition::Empty => {
            changed |=
                draw_empty_slot_card(ui, condition, rule_id, path, dragged_node, pending_drop);
        }
        BubbleStatusCondition::Field(field) => {
            let payload = BubbleStatusCondition::Field(*field);
            let mut clear_requested = false;
            Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(CONDITION_BLOCK_MIN_WIDTH_PX);
                ui.set_min_height(if is_root {
                    CONDITION_CARD_HEIGHT_PX
                } else {
                    68.0
                });
                let dragging_this = dragged_node
                    .as_ref()
                    .is_some_and(|dragged| dragged.rule_id == rule_id && dragged.path == path);
                let header = draw_condition_drag_bar(ui, "Поле", dragging_this);
                changed |= draw_field_pill(
                    ui,
                    &format!("field_{rule_id}_{path:?}"),
                    field,
                    dragging_this,
                );
                if header.drag_started {
                    *dragged_node = Some(DraggedBubbleConditionNode {
                        rule_id,
                        path: path.to_vec(),
                        payload: payload.clone(),
                    });
                }
                clear_requested = header.clear_requested;
                draw_drop_highlight_if_needed(ui, rule_id, path, dragged_node);
            });
            if clear_requested {
                *condition = BubbleStatusCondition::Empty;
                changed = true;
            }
        }
        BubbleStatusCondition::All(items) => {
            let payload = BubbleStatusCondition::All(items.clone());
            let mut clear_requested = false;
            let mut drag_started = false;
            let mut remove_child_idx = None;
            Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(CONDITION_BLOCK_MIN_WIDTH_PX);
                ui.set_min_height(if is_root {
                    CONDITION_CARD_HEIGHT_PX
                } else {
                    92.0
                });
                let dragging_this = dragged_node
                    .as_ref()
                    .is_some_and(|dragged| dragged.rule_id == rule_id && dragged.path == path);
                let items_len = items.len();
                draw_operator_condition_body(ui, dragging_this, |ui| {
                    let header = draw_condition_drag_bar(ui, "И", dragging_this);
                    drag_started = header.drag_started;
                    clear_requested = header.clear_requested;
                    ui.label(RichText::new("Все условия должны совпасть").color(Color32::WHITE));
                    ui.add_space(4.0);
                    for (child_idx, child) in items.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                let mut child_path = path.to_vec();
                                child_path.push(child_idx);
                                changed |= draw_condition_card(
                                    ui,
                                    child,
                                    rule_id,
                                    &child_path,
                                    false,
                                    dragged_node,
                                    pending_drop,
                                );
                            });
                            if items_len > 2 && ui.small_button("Удалить слот").clicked()
                            {
                                remove_child_idx = Some(child_idx);
                            }
                        });
                        if child_idx + 1 < items_len {
                            ui.add_space(4.0);
                            ui.label(RichText::new("И").color(Color32::WHITE));
                            ui.add_space(4.0);
                        }
                    }
                    ui.add_space(6.0);
                    if ui.small_button("Добавить пустой слот").clicked() {
                        items.push(BubbleStatusCondition::Empty);
                        changed = true;
                    }
                });
                if drag_started {
                    *dragged_node = Some(DraggedBubbleConditionNode {
                        rule_id,
                        path: path.to_vec(),
                        payload: payload.clone(),
                    });
                }
                draw_drop_highlight_if_needed(ui, rule_id, path, dragged_node);
            });
            if let Some(child_idx) = remove_child_idx {
                items.remove(child_idx);
                changed = true;
            }
            if clear_requested {
                *condition = BubbleStatusCondition::Empty;
                changed = true;
            }
        }
        BubbleStatusCondition::Any(items) => {
            let payload = BubbleStatusCondition::Any(items.clone());
            let mut clear_requested = false;
            let mut drag_started = false;
            let mut remove_child_idx = None;
            Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(CONDITION_BLOCK_MIN_WIDTH_PX);
                ui.set_min_height(if is_root {
                    CONDITION_CARD_HEIGHT_PX
                } else {
                    92.0
                });
                let dragging_this = dragged_node
                    .as_ref()
                    .is_some_and(|dragged| dragged.rule_id == rule_id && dragged.path == path);
                let items_len = items.len();
                draw_operator_condition_body(ui, dragging_this, |ui| {
                    let header = draw_condition_drag_bar(ui, "ИЛИ", dragging_this);
                    drag_started = header.drag_started;
                    clear_requested = header.clear_requested;
                    ui.label(RichText::new("Достаточно любого условия").color(Color32::WHITE));
                    ui.add_space(4.0);
                    for (child_idx, child) in items.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                let mut child_path = path.to_vec();
                                child_path.push(child_idx);
                                changed |= draw_condition_card(
                                    ui,
                                    child,
                                    rule_id,
                                    &child_path,
                                    false,
                                    dragged_node,
                                    pending_drop,
                                );
                            });
                            if items_len > 2 && ui.small_button("Удалить слот").clicked()
                            {
                                remove_child_idx = Some(child_idx);
                            }
                        });
                        if child_idx + 1 < items_len {
                            ui.add_space(4.0);
                            ui.label(RichText::new("ИЛИ").color(Color32::WHITE));
                            ui.add_space(4.0);
                        }
                    }
                    ui.add_space(6.0);
                    if ui.small_button("Добавить пустой слот").clicked() {
                        items.push(BubbleStatusCondition::Empty);
                        changed = true;
                    }
                });
                if drag_started {
                    *dragged_node = Some(DraggedBubbleConditionNode {
                        rule_id,
                        path: path.to_vec(),
                        payload: payload.clone(),
                    });
                }
                draw_drop_highlight_if_needed(ui, rule_id, path, dragged_node);
            });
            if let Some(child_idx) = remove_child_idx {
                items.remove(child_idx);
                changed = true;
            }
            if clear_requested {
                *condition = BubbleStatusCondition::Empty;
                changed = true;
            }
        }
        BubbleStatusCondition::Not(inner) => {
            let payload = BubbleStatusCondition::Not(Box::new((**inner).clone()));
            let mut clear_requested = false;
            let mut drag_started = false;
            Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_width(CONDITION_BLOCK_MIN_WIDTH_PX);
                ui.set_min_height(if is_root {
                    CONDITION_CARD_HEIGHT_PX
                } else {
                    84.0
                });
                let dragging_this = dragged_node
                    .as_ref()
                    .is_some_and(|dragged| dragged.rule_id == rule_id && dragged.path == path);
                draw_operator_condition_body(ui, dragging_this, |ui| {
                    let header = draw_condition_drag_bar(ui, "НЕ", dragging_this);
                    drag_started = header.drag_started;
                    clear_requested = header.clear_requested;
                    ui.label(RichText::new("Инвертирует вложенное условие").color(Color32::WHITE));
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.vertical(|ui| {
                            let mut child_path = path.to_vec();
                            child_path.push(0);
                            changed |= draw_condition_card(
                                ui,
                                inner,
                                rule_id,
                                &child_path,
                                false,
                                dragged_node,
                                pending_drop,
                            );
                        });
                    });
                });
                if drag_started {
                    *dragged_node = Some(DraggedBubbleConditionNode {
                        rule_id,
                        path: path.to_vec(),
                        payload: payload.clone(),
                    });
                }
                draw_drop_highlight_if_needed(ui, rule_id, path, dragged_node);
            });
            if clear_requested {
                *condition = BubbleStatusCondition::Empty;
                changed = true;
            }
        }
    }

    changed
}

#[derive(Debug, Clone, Copy)]
struct ConditionHeaderState {
    drag_started: bool,
    clear_requested: bool,
}

fn draw_condition_drag_bar(ui: &mut Ui, title: &str, dragging_this: bool) -> ConditionHeaderState {
    let mut drag_started = false;
    let mut clear_requested = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().color(Color32::WHITE));
        ui.add_space(6.0);
        let drag = draw_drag_handle(ui, dragging_this);
        drag_started = drag.drag_started();
        ui.add_space(4.0);
        let clear_label = "Удалить";
        if ui.small_button(clear_label).clicked() {
            clear_requested = true;
        }
    });
    ConditionHeaderState {
        drag_started,
        clear_requested,
    }
}

fn draw_empty_slot_card(
    ui: &mut Ui,
    condition: &mut BubbleStatusCondition,
    rule_id: u64,
    path: &[usize],
    dragged_node: &mut Option<DraggedBubbleConditionNode>,
    pending_drop: &mut Option<PendingConditionDrop>,
) -> bool {
    let mut changed = false;
    let can_accept_drop = dragged_node
        .as_ref()
        .is_some_and(|dragged| can_drop_condition_here(dragged, rule_id, path));

    let inner = Frame::new()
        .fill(Color32::from_rgb(42, 72, 42))
        .stroke(Stroke::new(
            if can_accept_drop { 2.0 } else { 1.0 },
            if can_accept_drop {
                Color32::from_rgb(160, 220, 255)
            } else {
                Color32::from_rgb(72, 150, 72)
            },
        ))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(CONDITION_BLOCK_MIN_WIDTH_PX - 16.0);
            ui.label(RichText::new("Пустой слот").color(Color32::WHITE));
            ui.label(
                RichText::new("Перетащите сюда блок или выберите тип условия.")
                    .color(Color32::WHITE),
            );
            ComboBox::from_id_salt(("empty_slot_choice", rule_id, path.to_vec()))
                .width(INLINE_SLOT_MIN_WIDTH_PX + 44.0)
                .selected_text("Выбрать")
                .show_ui(ui, |ui| {
                    if ui.button("Перевод заполнен").clicked() {
                        *condition =
                            BubbleStatusCondition::Field(BubbleStatusField::TranslationFilled);
                        changed = true;
                        ui.close();
                    }
                    if ui.button("Оригинал заполнен").clicked() {
                        *condition =
                            BubbleStatusCondition::Field(BubbleStatusField::OriginalFilled);
                        changed = true;
                        ui.close();
                    }
                    if ui.button("Персонаж заполнен").clicked() {
                        *condition =
                            BubbleStatusCondition::Field(BubbleStatusField::CharacterFilled);
                        changed = true;
                        ui.close();
                    }
                    if ui.button("Группа И").clicked() {
                        *condition = BubbleStatusCondition::All(empty_group_slots());
                        changed = true;
                        ui.close();
                    }
                    if ui.button("Группа ИЛИ").clicked() {
                        *condition = BubbleStatusCondition::Any(empty_group_slots());
                        changed = true;
                        ui.close();
                    }
                    if ui.button("НЕ").clicked() {
                        *condition =
                            BubbleStatusCondition::Not(Box::new(BubbleStatusCondition::Empty));
                        changed = true;
                        ui.close();
                    }
                });
        });

    let will_drop_here = can_accept_drop && inner.response.hovered();
    if will_drop_here {
        ui.painter().rect_stroke(
            inner.response.rect.expand(2.0),
            12.0,
            Stroke::new(2.0, Color32::WHITE),
            egui::StrokeKind::Outside,
        );
    }

    if will_drop_here
        && ui.ctx().input(|i| i.pointer.any_released())
        && let Some(dragged) = dragged_node.as_ref()
    {
        *pending_drop = Some(PendingConditionDrop {
            target_rule_id: rule_id,
            target_path: path.to_vec(),
            source_rule_id: dragged.rule_id,
            source_path: dragged.path.clone(),
            payload: dragged.payload.clone(),
        });
    }

    changed
}

fn draw_drop_highlight_if_needed(
    ui: &mut Ui,
    rule_id: u64,
    path: &[usize],
    dragged_node: &Option<DraggedBubbleConditionNode>,
) {
    let Some(dragged) = dragged_node.as_ref() else {
        return;
    };
    if !can_drop_condition_here(dragged, rule_id, path) {
        return;
    }
    let rect = ui.min_rect();
    if rect.contains(ui.ctx().pointer_hover_pos().unwrap_or(rect.min)) {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            10.0,
            Stroke::new(1.5, Color32::from_rgb(170, 225, 255)),
            egui::StrokeKind::Outside,
        );
    }
}

fn draw_drag_handle(ui: &mut Ui, dragging_this: bool) -> egui::Response {
    let label = if dragging_this {
        "Перетаскивается"
    } else {
        "Перетащить"
    };
    ui.add(
        egui::Button::new(label)
            .sense(Sense::click_and_drag())
            .small(),
    )
}

fn draw_operator_condition_body(
    ui: &mut Ui,
    dragging_this: bool,
    add_children: impl FnOnce(&mut Ui),
) {
    let inner = Frame::new()
        .fill(if dragging_this {
            Color32::from_rgb(86, 186, 86)
        } else {
            Color32::from_rgb(64, 156, 64)
        })
        .stroke(Stroke::new(1.5, Color32::from_rgb(120, 210, 120)))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                add_children(ui);
            });
        });
    let _ = inner;
}

fn draw_field_pill(
    ui: &mut Ui,
    id_source: &str,
    field: &mut BubbleStatusField,
    dragging_this: bool,
) -> bool {
    let fill = if dragging_this {
        Color32::from_rgb(88, 170, 210)
    } else {
        Color32::from_rgb(78, 160, 200)
    };
    let mut changed = false;
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, Color32::from_rgb(120, 200, 230)))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ComboBox::from_id_salt(id_source)
                .width(INLINE_SLOT_MIN_WIDTH_PX)
                .selected_text(field_dropdown_label(*field))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            field,
                            BubbleStatusField::TranslationFilled,
                            field_dropdown_label(BubbleStatusField::TranslationFilled),
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            field,
                            BubbleStatusField::OriginalFilled,
                            field_dropdown_label(BubbleStatusField::OriginalFilled),
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            field,
                            BubbleStatusField::CharacterFilled,
                            field_dropdown_label(BubbleStatusField::CharacterFilled),
                        )
                        .changed();
                });
        });
    changed
}

fn can_drop_condition_here(
    dragged: &DraggedBubbleConditionNode,
    rule_id: u64,
    path: &[usize],
) -> bool {
    if dragged.rule_id == 0 {
        return true;
    }
    if dragged.rule_id != rule_id {
        return true;
    }
    dragged.path != path && !path_starts_with(path, &dragged.path)
}

fn path_starts_with(path: &[usize], prefix: &[usize]) -> bool {
    path.len() >= prefix.len() && path.iter().zip(prefix.iter()).all(|(a, b)| a == b)
}

fn apply_condition_drop(rules: &mut [BubbleStatusRule], drop: PendingConditionDrop) -> bool {
    let Some(target_rule) = rules.iter_mut().find(|rule| rule.id == drop.target_rule_id) else {
        return false;
    };
    let Some(target) = get_condition_mut(&mut target_rule.condition, &drop.target_path) else {
        return false;
    };
    *target = drop.payload;

    if drop.source_rule_id == 0 {
        return true;
    }

    let Some(source_rule) = rules.iter_mut().find(|rule| rule.id == drop.source_rule_id) else {
        return true;
    };
    let Some(source) = get_condition_mut(&mut source_rule.condition, &drop.source_path) else {
        return true;
    };
    *source = BubbleStatusCondition::Empty;
    true
}

fn get_condition_mut<'a>(
    condition: &'a mut BubbleStatusCondition,
    path: &[usize],
) -> Option<&'a mut BubbleStatusCondition> {
    if path.is_empty() {
        return Some(condition);
    }

    match condition {
        BubbleStatusCondition::All(items) | BubbleStatusCondition::Any(items) => {
            let (idx, rest) = path.split_first()?;
            get_condition_mut(items.get_mut(*idx)?, rest)
        }
        BubbleStatusCondition::Not(inner) => {
            let (idx, rest) = path.split_first()?;
            if *idx != 0 {
                return None;
            }
            get_condition_mut(inner.as_mut(), rest)
        }
        BubbleStatusCondition::Empty | BubbleStatusCondition::Field(_) => None,
    }
}

fn field_dropdown_label(field: BubbleStatusField) -> &'static str {
    match field {
        BubbleStatusField::TranslationFilled => "Перевод заполнен",
        BubbleStatusField::OriginalFilled => "Оригинал заполнен",
        BubbleStatusField::CharacterFilled => "Персонаж заполнен",
    }
}

fn draw_dragged_condition_preview(
    ctx: &egui::Context,
    dragged_node: Option<&DraggedBubbleConditionNode>,
) {
    let Some(dragged_node) = dragged_node else {
        return;
    };
    let Some(pointer_pos) = ctx.pointer_hover_pos() else {
        return;
    };

    ctx.request_repaint();

    let rect = Rect::from_min_size(
        Pos2::new(pointer_pos.x + 16.0, pointer_pos.y + 16.0),
        vec2(220.0, 72.0),
    );
    let layer_id = LayerId::new(Order::Tooltip, Id::new("bubble_condition_drag_preview"));
    let painter = ctx.layer_painter(layer_id);

    painter.rect_filled(rect, 8.0, Color32::from_rgba_premultiplied(26, 26, 30, 235));
    painter.rect_stroke(
        rect,
        8.0,
        Stroke::new(2.0, Color32::WHITE),
        egui::StrokeKind::Outside,
    );
    painter.text(
        rect.left_top() + vec2(12.0, 12.0),
        Align2::LEFT_TOP,
        dragged_condition_title(&dragged_node.payload),
        FontId::proportional(16.0),
        Color32::WHITE,
    );
    painter.text(
        rect.left_top() + vec2(12.0, 36.0),
        Align2::LEFT_TOP,
        dragged_node.payload.summary(),
        FontId::proportional(13.0),
        Color32::from_gray(210),
    );
}

fn dragged_condition_title(condition: &BubbleStatusCondition) -> &'static str {
    match condition {
        BubbleStatusCondition::Empty => "Поле",
        BubbleStatusCondition::Field(_) => "Поле",
        BubbleStatusCondition::All(_) => "И",
        BubbleStatusCondition::Any(_) => "ИЛИ",
        BubbleStatusCondition::Not(_) => "НЕ",
    }
}
#[allow(dead_code)]
fn default_user_rule(id: u64) -> BubbleStatusRule {
    BubbleStatusRule {
        id,
        condition: BubbleStatusCondition::Empty,
        border: BubbleBorderStyle::new(BubbleBorderKind::Dashed, Color32::from_rgb(120, 170, 255)),
    }
}
