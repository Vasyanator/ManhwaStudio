/*
FILE OVERVIEW: src/tabs/translation/panels/bubbles.rs
Translation tab panel with searchable/editable bubble cards and debounced text/footer syncing.

Main types:
- `BubbleFooterState`: editable footer metadata snapshot stored per bubble.
- `BubblesSearchScope`: search target (`All`, `Original`, `Translation`).
- `BubblesPanelFilters`: draft/applied search filters (query/page/character/scope).
- `BubblePanelEditorState`: per-bubble editor mirror for text/footer fields and lowercase caches.
- `BubblesPanelState`: panel runtime state (filters, editor cache, pending flushes, visible list cache).
- `BubblesPanelContext`: shared tab state links (footer overrides, pending patches, character refresh flags).

Main flow:
- `draw_bubbles_panel` -> `BubblesPanelState::draw`: renders controls, applies filters, draws card list.
- `flush_text_updates`: debounced write-through of text/original text into `CanvasView`.
- `draw_card`: renders one bubble card, queues footer patches, and exposes card-level context menu actions.
- filter/cache helpers: `sync_editor_from_project`, `character_options`, `ensure_visible_cache`, `matches_filters`.

Utilities:
- footer parsing helpers: `footer_state_for_bubble`, `bubble_footer_state_from_record`,
  `bubble_extra_string` (без trim, сохраняет пользовательские пробелы),
  `bubble_extra_bool`, `bubble_extra_i32`.
- patch helper: `queue_footer_patch`.
*/

use crate::canvas::{BubbleTextField, CanvasView};
use crate::project::{Bubble, ProjectData};
use crate::widgets::{WheelComboBox, WheelSpinBox};
use eframe::egui;
use egui::{Color32, Stroke};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub const FOOTER_NO_CHARACTER: &str = "(не указан)";
pub const FOOTER_NO_CHARACTERS: &str = "(нет персонажей)";
const BUBBLES_PANEL_TEXT_DEBOUNCE_SECS: f64 = 0.30;

#[derive(Debug, Clone)]
pub struct BubbleFooterState {
    pub bubble_order: i32,
    pub is_known_character: bool,
    pub character_name: String,
    pub clarification: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum BubblesSearchScope {
    #[default]
    All,
    Original,
    Translation,
}

impl BubblesSearchScope {
    fn title(self) -> &'static str {
        match self {
            BubblesSearchScope::All => "Везде",
            BubblesSearchScope::Original => "Оригинал",
            BubblesSearchScope::Translation => "Перевод",
        }
    }
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
struct BubblesPanelFilters {
    query: String,
    page: Option<usize>,
    character: Option<String>,
    scope: BubblesSearchScope,
}

#[derive(Debug, Clone)]
struct BubblePanelEditorState {
    text: String,
    original_text: String,
    bubble_order: i32,
    is_known_character: bool,
    character_name: String,
    clarification: String,
    text_lc: String,
    original_text_lc: String,
    character_name_lc: String,
}

impl BubblePanelEditorState {
    fn from_bubble(bubble: &Bubble, footer: BubbleFooterState) -> Self {
        Self {
            text: bubble.text.clone(),
            original_text: bubble.original_text.clone(),
            bubble_order: footer.bubble_order,
            is_known_character: footer.is_known_character,
            character_name: footer.character_name.clone(),
            clarification: footer.clarification,
            text_lc: bubble.text.to_lowercase(),
            original_text_lc: bubble.original_text.to_lowercase(),
            character_name_lc: footer.character_name.to_lowercase(),
        }
    }

    fn footer_state(&self) -> BubbleFooterState {
        BubbleFooterState {
            bubble_order: self.bubble_order,
            is_known_character: self.is_known_character,
            character_name: self.character_name.clone(),
            clarification: self.clarification.clone(),
        }
    }

    fn refresh_text_lc(&mut self) {
        self.text_lc = self.text.to_lowercase();
    }

    fn refresh_original_text_lc(&mut self) {
        self.original_text_lc = self.original_text.to_lowercase();
    }

    fn refresh_character_name_lc(&mut self) {
        self.character_name_lc = self.character_name.to_lowercase();
    }

    fn sync_texts_from_project(&mut self, text: &str, original_text: &str) -> bool {
        let mut changed = false;
        if self.text != text {
            self.text.clear();
            self.text.push_str(text);
            self.refresh_text_lc();
            changed = true;
        }
        if self.original_text != original_text {
            self.original_text.clear();
            self.original_text.push_str(original_text);
            self.refresh_original_text_lc();
            changed = true;
        }
        changed
    }

    fn sync_footer_from_state(&mut self, footer: BubbleFooterState) -> bool {
        let mut changed = false;
        if self.bubble_order != footer.bubble_order {
            self.bubble_order = footer.bubble_order;
            changed = true;
        }
        if self.is_known_character != footer.is_known_character {
            self.is_known_character = footer.is_known_character;
            changed = true;
        }
        if self.character_name != footer.character_name {
            self.character_name = footer.character_name;
            self.refresh_character_name_lc();
            changed = true;
        }
        if self.clarification != footer.clarification {
            self.clarification = footer.clarification;
            changed = true;
        }
        changed
    }
}

#[derive(Debug)]
pub struct BubblesPanelState {
    filters_draft: BubblesPanelFilters,
    filters_applied: BubblesPanelFilters,
    filters_applied_query_lc: String,
    filters_applied_character_lc: Option<String>,
    editor: HashMap<i64, BubblePanelEditorState>,
    pending_text_flush_at: HashMap<i64, f64>,
    visible_cache: Vec<usize>,
    visible_cache_dirty: bool,
    character_options_cache: Vec<String>,
    character_options_dirty: bool,
}

impl Default for BubblesPanelState {
    fn default() -> Self {
        Self {
            filters_draft: BubblesPanelFilters::default(),
            filters_applied: BubblesPanelFilters::default(),
            filters_applied_query_lc: String::new(),
            filters_applied_character_lc: None,
            editor: HashMap::new(),
            pending_text_flush_at: HashMap::new(),
            visible_cache: Vec::new(),
            visible_cache_dirty: true,
            character_options_cache: Vec::new(),
            character_options_dirty: true,
        }
    }
}

pub struct BubblesPanelContext<'a> {
    pub character_names: &'a [String],
    pub footer_overrides: &'a mut HashMap<i64, BubbleFooterState>,
    pub pending_footer_patches: &'a mut HashMap<i64, Map<String, Value>>,
    pub pending_footer_patch_changed_at: &'a mut HashMap<i64, f64>,
    pub pending_characters_refresh: &'a mut bool,
    pub last_is_known_character: &'a mut bool,
    pub last_character_name: &'a mut String,
    pub last_clarification: &'a mut String,
    pub last_page_idx: &'a mut i64,
    pub last_bubble_order: &'a mut i32,
}

pub fn draw_bubbles_panel(
    state: &mut BubblesPanelState,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    canvas: &mut CanvasView,
    project: &ProjectData,
    panel_ctx: &mut BubblesPanelContext<'_>,
) {
    state.draw(ui, ctx, canvas, project, panel_ctx);
}

impl BubblesPanelState {
    pub fn flush_text_updates(&mut self, canvas: &mut CanvasView, now_s: f64) {
        if self.pending_text_flush_at.is_empty() {
            return;
        }
        let ready = self
            .pending_text_flush_at
            .iter()
            .filter_map(|(bubble_id, changed_at)| {
                if now_s - *changed_at >= BUBBLES_PANEL_TEXT_DEBOUNCE_SECS {
                    Some(*bubble_id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for bubble_id in ready {
            if let Some(editor) = self.editor.get(&bubble_id) {
                let _ = canvas.set_bubble_texts_from_panel(
                    bubble_id,
                    Some(editor.text.clone()),
                    Some(editor.original_text.clone()),
                    now_s,
                    true,
                );
            }
            self.pending_text_flush_at.remove(&bubble_id);
        }
    }

    pub fn has_pending_text_updates(&self) -> bool {
        !self.pending_text_flush_at.is_empty()
    }

    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        panel_ctx: &mut BubblesPanelContext<'_>,
    ) {
        if self.sync_editor_from_project(project, panel_ctx.footer_overrides) {
            self.visible_cache_dirty = true;
            self.character_options_dirty = true;
        }

        ui.horizontal(|ui| {
            if ui.button("Обновить").clicked() {
                self.editor.clear();
                self.pending_text_flush_at.clear();
                self.visible_cache_dirty = true;
                self.character_options_dirty = true;
                if self.sync_editor_from_project(project, panel_ctx.footer_overrides) {
                    self.visible_cache_dirty = true;
                    self.character_options_dirty = true;
                }
            }
        });

        ui.add_space(2.0);
        let character_options = self.character_options(project).to_vec();
        let draft_before = self.filters_draft.clone();

        ui.horizontal_wrapped(|ui| {
            let character_title = self
                .filters_draft
                .character
                .as_ref()
                .cloned()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "Все персонажи".to_string());
            WheelComboBox::from_id_salt("translation_bubbles_panel_character_filter")
                .selected_text(character_title)
                .width(170.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filters_draft.character, None, "Все персонажи");
                    for name in &character_options {
                        ui.selectable_value(
                            &mut self.filters_draft.character,
                            Some(name.clone()),
                            name,
                        );
                    }
                });

            WheelComboBox::from_id_salt("translation_bubbles_panel_scope_filter")
                .selected_text(self.filters_draft.scope.title())
                .width(120.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.filters_draft.scope,
                        BubblesSearchScope::All,
                        BubblesSearchScope::All.title(),
                    );
                    ui.selectable_value(
                        &mut self.filters_draft.scope,
                        BubblesSearchScope::Original,
                        BubblesSearchScope::Original.title(),
                    );
                    ui.selectable_value(
                        &mut self.filters_draft.scope,
                        BubblesSearchScope::Translation,
                        BubblesSearchScope::Translation.title(),
                    );
                });
        });

        ui.horizontal_wrapped(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filters_draft.query)
                    .desired_width(180.0)
                    .hint_text("Поиск..."),
            );

            let page_title = self
                .filters_draft
                .page
                .map(|idx| format!("Страница #{}", idx + 1))
                .unwrap_or_else(|| "Все страницы".to_string());
            WheelComboBox::from_id_salt("translation_bubbles_panel_page_filter")
                .selected_text(page_title)
                .width(140.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filters_draft.page, None, "Все страницы");
                    for idx in 0..project.pages.len() {
                        ui.selectable_value(
                            &mut self.filters_draft.page,
                            Some(idx),
                            format!("Страница #{}", idx + 1),
                        );
                    }
                });
        });

        if self.filters_draft != draft_before {
            self.filters_applied.query = self.filters_draft.query.trim().to_string();
            self.filters_applied.page = self.filters_draft.page;
            self.filters_applied.scope = self.filters_draft.scope;
            self.filters_applied.character = self
                .filters_draft
                .character
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            self.filters_applied_query_lc = self.filters_applied.query.to_lowercase();
            self.filters_applied_character_lc = self
                .filters_applied
                .character
                .as_ref()
                .map(|value| value.to_lowercase());
            self.visible_cache_dirty = true;
        }

        ui.separator();
        self.ensure_visible_cache(project);

        ui.label(format!(
            "Показано пузырей: {} / {}",
            self.visible_cache.len(),
            project.bubbles.len()
        ));
        ui.add_space(4.0);

        let now_s = ctx.input(|i| i.time);
        let visible_indices = self.visible_cache.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for bubble_idx in visible_indices.iter().copied() {
                    let Some(bubble) = project.bubbles.get(bubble_idx) else {
                        continue;
                    };
                    self.draw_card(ui, ctx, canvas, project, bubble, now_s, panel_ctx);
                    ui.add_space(8.0);
                }
            });
    }

    // Parameters represent distinct required inputs with no natural grouping.
    #[allow(clippy::too_many_arguments)]
    fn draw_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        bubble: &Bubble,
        now_s: f64,
        panel_ctx: &mut BubblesPanelContext<'_>,
    ) {
        let bubble_id = bubble.id;
        let footer = footer_state_for_bubble(panel_ctx.footer_overrides, bubble);
        let placed = bubble.side.is_some() && bubble.img_idx < project.pages.len();
        let mut translation_changed = false;
        let mut original_changed = false;
        let mut text_search_dirty = false;
        let mut character_dirty = false;
        let mut move_clicked = false;
        let mut copy_original_clicked = false;
        let mut copy_translation_clicked = false;
        let mut paste_original_clicked = false;
        let mut paste_translation_clicked = false;
        let mut translate_clicked = false;
        let mut delete_clicked = false;
        let mut footer_state;
        let allow_paste = canvas.editable;

        {
            let editor = self
                .editor
                .entry(bubble_id)
                .or_insert_with(|| BubblePanelEditorState::from_bubble(bubble, footer));

            let card_response = egui::Frame::new()
                .fill(Color32::from_rgb(31, 31, 31))
                .stroke(Stroke::new(1.0, Color32::from_gray(60)))
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if placed {
                            ui.strong(format!("Изображение #{}", bubble.img_idx.saturating_add(1)));
                        } else {
                            ui.colored_label(Color32::from_rgb(208, 84, 62), "Не привязан");
                        }
                        ui.label(format!("#{}", bubble_id));
                    });

                    ui.add_space(4.0);
                    let translated_resp = ui.add(
                        egui::TextEdit::multiline(&mut editor.text)
                            .desired_rows(3)
                            .hint_text("Перевод"),
                    );
                    if translated_resp.changed() {
                        translation_changed = true;
                        text_search_dirty = true;
                        editor.refresh_text_lc();
                    }

                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Оригинал:").small());
                    let original_resp = ui.add(
                        egui::TextEdit::multiline(&mut editor.original_text)
                            .desired_rows(2)
                            .hint_text("Оригинальный текст..."),
                    );
                    if original_resp.changed() {
                        original_changed = true;
                        text_search_dirty = true;
                        editor.refresh_original_text_lc();
                    }

                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Порядок:");
                        let mut bubble_order = editor.bubble_order;
                        let order_resp = ui.add(
                            WheelSpinBox::new(&mut bubble_order)
                                .range(0..=100_000)
                                .speed(0.25),
                        );
                        if order_resp.changed() {
                            editor.bubble_order = bubble_order.clamp(0, 100_000);
                            queue_footer_patch(
                                panel_ctx.pending_footer_patches,
                                panel_ctx.pending_footer_patch_changed_at,
                                bubble_id,
                                "bubble_order",
                                Value::Number(editor.bubble_order.into()),
                                now_s,
                            );
                            *panel_ctx.last_page_idx = bubble.img_idx as i64;
                            *panel_ctx.last_bubble_order = editor.bubble_order;
                        }

                        let known_resp = ui
                            .checkbox(&mut editor.is_known_character, "И.П.")
                            .on_hover_text(
                                "Использовать готовые имена персонажей, или ввести своё.",
                            );
                        if known_resp.changed() {
                            queue_footer_patch(
                                panel_ctx.pending_footer_patches,
                                panel_ctx.pending_footer_patch_changed_at,
                                bubble_id,
                                "is_known_character",
                                Value::Bool(editor.is_known_character),
                                now_s,
                            );
                            let prev_character_name = editor.character_name.clone();
                            if editor.is_known_character {
                                if panel_ctx.character_names.is_empty() {
                                    editor.character_name.clear();
                                } else if editor.character_name == FOOTER_NO_CHARACTERS
                                    || !panel_ctx
                                        .character_names
                                        .iter()
                                        .any(|item| item == &editor.character_name)
                                {
                                    editor.character_name = panel_ctx.character_names[0].clone();
                                }
                                if editor.character_name == FOOTER_NO_CHARACTERS {
                                    editor.character_name.clear();
                                }
                            }
                            if editor.character_name != prev_character_name {
                                editor.refresh_character_name_lc();
                                character_dirty = true;
                            }
                            queue_footer_patch(
                                panel_ctx.pending_footer_patches,
                                panel_ctx.pending_footer_patch_changed_at,
                                bubble_id,
                                "character_name",
                                Value::String(editor.character_name.clone()),
                                now_s,
                            );
                            *panel_ctx.last_is_known_character = editor.is_known_character;
                            *panel_ctx.last_character_name = editor.character_name.clone();
                        }
                    });

                    ui.horizontal_wrapped(|ui| {
                        if editor.is_known_character {
                            if panel_ctx.character_names.is_empty() {
                                ui.label(FOOTER_NO_CHARACTERS);
                            } else {
                                let mut selected_name = editor.character_name.clone();
                                WheelComboBox::from_id_salt((
                                    "translation_bubbles_panel_character",
                                    bubble_id,
                                ))
                                .selected_text(if selected_name.trim().is_empty() {
                                    FOOTER_NO_CHARACTER.to_string()
                                } else {
                                    selected_name.clone()
                                })
                                .width(160.0)
                                .show_ui(ui, |ui| {
                                    for item in panel_ctx.character_names {
                                        ui.selectable_value(&mut selected_name, item.clone(), item);
                                    }
                                });
                                if selected_name != editor.character_name {
                                    editor.character_name = selected_name;
                                    editor.refresh_character_name_lc();
                                    character_dirty = true;
                                    queue_footer_patch(
                                        panel_ctx.pending_footer_patches,
                                        panel_ctx.pending_footer_patch_changed_at,
                                        bubble_id,
                                        "character_name",
                                        Value::String(editor.character_name.clone()),
                                        now_s,
                                    );
                                    if !editor.clarification.is_empty() {
                                        editor.clarification.clear();
                                        queue_footer_patch(
                                            panel_ctx.pending_footer_patches,
                                            panel_ctx.pending_footer_patch_changed_at,
                                            bubble_id,
                                            "clarification",
                                            Value::String(String::new()),
                                            now_s,
                                        );
                                    }
                                    *panel_ctx.last_character_name = editor.character_name.clone();
                                    panel_ctx.last_clarification.clear();
                                }
                            }

                            if ui
                                .small_button("↻")
                                .on_hover_text("Обновить список персонажей из characters.json")
                                .clicked()
                            {
                                *panel_ctx.pending_characters_refresh = true;
                            }

                            let clarification_resp = ui.add(
                                egui::TextEdit::singleline(&mut editor.clarification)
                                    .hint_text("Уточнение...")
                                    .desired_width(150.0),
                            );
                            if clarification_resp.changed() {
                                queue_footer_patch(
                                    panel_ctx.pending_footer_patches,
                                    panel_ctx.pending_footer_patch_changed_at,
                                    bubble_id,
                                    "clarification",
                                    Value::String(editor.clarification.clone()),
                                    now_s,
                                );
                                *panel_ctx.last_clarification = editor.clarification.clone();
                            }
                        } else {
                            let character_resp = ui.add(
                                egui::TextEdit::singleline(&mut editor.character_name)
                                    .hint_text("Имя персонажа...")
                                    .desired_width(180.0),
                            );
                            if character_resp.changed() {
                                editor.refresh_character_name_lc();
                                character_dirty = true;
                                queue_footer_patch(
                                    panel_ctx.pending_footer_patches,
                                    panel_ctx.pending_footer_patch_changed_at,
                                    bubble_id,
                                    "character_name",
                                    Value::String(editor.character_name.clone()),
                                    now_s,
                                );
                                *panel_ctx.last_character_name = editor.character_name.clone();
                            }
                        }
                    });

                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        let move_btn = if canvas.is_bubble_move_mode_active(bubble_id) {
                            "Отменить перемещение"
                        } else if placed {
                            "Переместить"
                        } else {
                            "Разместить"
                        };
                        if ui.small_button(move_btn).clicked() {
                            move_clicked = true;
                        }
                        if ui.small_button("Перевести").clicked() {
                            translate_clicked = true;
                        }
                        if ui.small_button("Удалить").clicked() {
                            delete_clicked = true;
                        }
                    });
                })
                .response;

            card_response.context_menu(|ui| {
                if ui.button("Копировать оригинал").clicked() {
                    copy_original_clicked = true;
                    ui.close();
                }
                if ui.button("Копировать перевод").clicked() {
                    copy_translation_clicked = true;
                    ui.close();
                }
                ui.separator();
                if ui
                    .add_enabled(allow_paste, egui::Button::new("Вставить в оригинал"))
                    .clicked()
                {
                    paste_original_clicked = true;
                    ui.close();
                }
                if ui
                    .add_enabled(allow_paste, egui::Button::new("Вставить в перевод"))
                    .clicked()
                {
                    paste_translation_clicked = true;
                    ui.close();
                }
            });
            footer_state = editor.footer_state();
        }

        if copy_original_clicked {
            let _ = canvas.copy_bubble_text_to_clipboard(ctx, bubble_id, BubbleTextField::Original);
        }
        if copy_translation_clicked {
            let _ =
                canvas.copy_bubble_text_to_clipboard(ctx, bubble_id, BubbleTextField::Translation);
        }
        if paste_original_clicked
            && let Some(pasted_text) =
                canvas.paste_bubble_text_from_clipboard(ctx, bubble_id, BubbleTextField::Original)
        {
            if let Some(editor) = self.editor.get_mut(&bubble_id) {
                editor.original_text = pasted_text;
                editor.refresh_original_text_lc();
                footer_state = editor.footer_state();
            }
            original_changed = true;
            text_search_dirty = true;
        }
        if paste_translation_clicked {
            let mut changed = false;
            if let Some(pasted_text) = canvas.paste_bubble_text_from_clipboard(
                ctx,
                bubble_id,
                BubbleTextField::Translation,
            ) {
                if let Some(editor) = self.editor.get_mut(&bubble_id) {
                    editor.text = pasted_text;
                    editor.refresh_text_lc();
                    footer_state = editor.footer_state();
                }
                translation_changed = true;
                text_search_dirty = true;
                changed = true;
            }
            if changed {
                queue_footer_patch(
                    panel_ctx.pending_footer_patches,
                    panel_ctx.pending_footer_patch_changed_at,
                    bubble_id,
                    "translation_status",
                    Value::String("translated".to_string()),
                    now_s,
                );
            }
        }

        if translation_changed || original_changed {
            self.pending_text_flush_at.insert(bubble_id, now_s);
        }
        if text_search_dirty {
            self.visible_cache_dirty = true;
        }

        if move_clicked {
            let _ = canvas.toggle_move_mode_for_bubble(bubble_id);
        }
        if translate_clicked {
            let _ = canvas.request_translate_bubble(bubble_id);
        }
        if delete_clicked {
            let _ = canvas.request_delete_bubble(bubble_id);
        }
        if character_dirty {
            self.visible_cache_dirty = true;
            self.character_options_dirty = true;
        }

        panel_ctx.footer_overrides.insert(bubble_id, footer_state);
    }

    fn sync_editor_from_project(
        &mut self,
        project: &ProjectData,
        footer_overrides: &HashMap<i64, BubbleFooterState>,
    ) -> bool {
        let mut changed = false;
        let mut alive = HashSet::with_capacity(project.bubbles.len());
        for bubble in project.bubbles.iter() {
            let bubble_id = bubble.id;
            alive.insert(bubble_id);
            let footer = footer_state_for_bubble(footer_overrides, bubble);
            if let Some(editor) = self.editor.get_mut(&bubble_id) {
                if !self.pending_text_flush_at.contains_key(&bubble_id)
                    && editor.sync_texts_from_project(&bubble.text, &bubble.original_text)
                {
                    changed = true;
                }
                if editor.sync_footer_from_state(footer) {
                    changed = true;
                }
                continue;
            }
            self.editor.insert(
                bubble_id,
                BubblePanelEditorState::from_bubble(bubble, footer),
            );
            changed = true;
        }

        let editor_len_before = self.editor.len();
        self.editor.retain(|bubble_id, _| alive.contains(bubble_id));
        if self.editor.len() != editor_len_before {
            changed = true;
        }

        let pending_len_before = self.pending_text_flush_at.len();
        self.pending_text_flush_at
            .retain(|bubble_id, _| alive.contains(bubble_id));
        if self.pending_text_flush_at.len() != pending_len_before {
            changed = true;
        }
        changed
    }

    fn character_options(&mut self, project: &ProjectData) -> &[String] {
        if self.character_options_dirty {
            self.rebuild_character_options_cache(project);
        }
        &self.character_options_cache
    }

    fn rebuild_character_options_cache(&mut self, project: &ProjectData) {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for bubble in project.bubbles.iter() {
            let name = self
                .editor
                .get(&bubble.id)
                .map(|editor| editor.character_name.clone())
                .unwrap_or_else(|| bubble_extra_string(&bubble.extra, "character_name"));
            let trimmed = name.trim();
            if trimmed.is_empty() {
                continue;
            }
            let key = trimmed.to_lowercase();
            if seen.insert(key) {
                out.push(trimmed.to_string());
            }
        }
        out.sort_by_key(|value| value.to_lowercase());
        self.character_options_cache = out;
        self.character_options_dirty = false;
    }

    fn ensure_visible_cache(&mut self, project: &ProjectData) {
        if !self.visible_cache_dirty {
            return;
        }
        self.visible_cache.clear();
        for (idx, bubble) in project.bubbles.iter().enumerate() {
            if self
                .editor
                .get(&bubble.id)
                .is_some_and(|editor| self.matches_filters(bubble, editor))
            {
                self.visible_cache.push(idx);
            }
        }
        self.visible_cache
            .sort_by_key(|idx| project.bubbles[*idx].id);
        self.visible_cache_dirty = false;
    }

    fn matches_filters(&self, bubble: &Bubble, editor: &BubblePanelEditorState) -> bool {
        if let Some(page) = self.filters_applied.page
            && bubble.img_idx != page
        {
            return false;
        }

        if let Some(character_lc) = self.filters_applied_character_lc.as_ref()
            && editor.character_name_lc != *character_lc
        {
            return false;
        }

        if self.filters_applied_query_lc.is_empty() {
            return true;
        }
        match self.filters_applied.scope {
            BubblesSearchScope::All => {
                editor
                    .original_text_lc
                    .contains(&self.filters_applied_query_lc)
                    || editor.text_lc.contains(&self.filters_applied_query_lc)
            }
            BubblesSearchScope::Original => editor
                .original_text_lc
                .contains(&self.filters_applied_query_lc),
            BubblesSearchScope::Translation => {
                editor.text_lc.contains(&self.filters_applied_query_lc)
            }
        }
    }
}

fn queue_footer_patch(
    pending_footer_patches: &mut HashMap<i64, Map<String, Value>>,
    pending_footer_patch_changed_at: &mut HashMap<i64, f64>,
    bubble_id: i64,
    field: &str,
    value: Value,
    now_s: f64,
) {
    pending_footer_patches
        .entry(bubble_id)
        .or_default()
        .insert(field.to_string(), value);
    pending_footer_patch_changed_at.insert(bubble_id, now_s);
}

pub fn footer_state_for_bubble(
    footer_overrides: &HashMap<i64, BubbleFooterState>,
    bubble: &Bubble,
) -> BubbleFooterState {
    footer_overrides
        .get(&bubble.id)
        .cloned()
        .unwrap_or_else(|| bubble_footer_state_from_record(bubble))
}

pub fn bubble_footer_state_from_record(bubble: &Bubble) -> BubbleFooterState {
    BubbleFooterState {
        bubble_order: bubble_extra_i32(&bubble.extra, "bubble_order", 0).clamp(0, 100_000),
        is_known_character: bubble_extra_bool(&bubble.extra, "is_known_character", true),
        character_name: bubble_extra_string(&bubble.extra, "character_name"),
        clarification: bubble_extra_string(&bubble.extra, "clarification"),
    }
}

pub fn bubble_extra_string(extra: &Map<String, Value>, key: &str) -> String {
    extra
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn bubble_extra_bool(extra: &Map<String, Value>, key: &str, default: bool) -> bool {
    let Some(raw) = extra.get(key) else {
        return default;
    };
    match raw {
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_i64().is_some_and(|iv| iv != 0),
        Value::String(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        _ => default,
    }
}

pub fn bubble_extra_i32(extra: &Map<String, Value>, key: &str, default: i32) -> i32 {
    let Some(raw) = extra.get(key) else {
        return default;
    };
    match raw {
        Value::Number(v) => v
            .as_i64()
            .and_then(|iv| i32::try_from(iv).ok())
            .unwrap_or(default),
        Value::String(v) => v.trim().parse::<i32>().unwrap_or(default),
        _ => default,
    }
}
