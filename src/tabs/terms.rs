/*
FILE OVERVIEW: src/tabs/terms.rs
Terms tab state and CRUD UI for project-scoped `terms.json`.

Main items:
- `TermsTabState`: cached list/filter/editor state, with lazy reload per active project.
- `TermEntry`: persisted term schema (`name`, `orig_name`, `description`, `tags`).
- Editor/confirm windows: add/edit/delete flows and overwrite confirmation.

Storage behavior:
- Reads/writes `project.paths.terms_file`.
- Supports legacy `tags` wire format as string or string array.
- Keeps names/tags normalized and sorted, with case-insensitive dedupe.
*/

use crate::project::ProjectData;
use crate::widgets::WheelComboBox;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const TAG_ALL: &str = "(все)";
const MAX_NAME_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct TermNoteEntry {
    pub name: String,
    pub orig_name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TermEntry {
    name: String,
    #[serde(default)]
    orig_name: String,
    #[serde(default)]
    description: String,
    #[serde(default, deserialize_with = "deserialize_tags")]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TagsWire {
    One(String),
    Many(Vec<String>),
}

fn deserialize_tags<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let parsed = Option::<TagsWire>::deserialize(deserializer)?;
    Ok(match parsed {
        Some(TagsWire::One(v)) => vec![v],
        Some(TagsWire::Many(v)) => v,
        None => Vec::new(),
    })
}

#[derive(Debug, Clone)]
enum EditorMode {
    Add,
    Edit { original_name: String },
}

#[derive(Debug, Clone)]
struct TermEditorState {
    mode: EditorMode,
    name: String,
    orig_name: String,
    description: String,
    tags: Vec<String>,
    tag_input: String,
    open: bool,
}

impl TermEditorState {
    fn for_add(available_tags: &[String]) -> Self {
        Self {
            mode: EditorMode::Add,
            name: String::new(),
            orig_name: String::new(),
            description: String::new(),
            tags: Vec::new(),
            tag_input: available_tags.first().cloned().unwrap_or_default(),
            open: true,
        }
    }

    fn for_edit(entry: &TermEntry, available_tags: &[String]) -> Self {
        Self {
            mode: EditorMode::Edit {
                original_name: entry.name.clone(),
            },
            name: entry.name.clone(),
            orig_name: entry.orig_name.clone(),
            description: entry.description.clone(),
            tags: entry.tags.clone(),
            tag_input: available_tags.first().cloned().unwrap_or_default(),
            open: true,
        }
    }

    fn title(&self) -> &'static str {
        match self.mode {
            EditorMode::Add => "Добавить термин",
            EditorMode::Edit { .. } => "Редактировать термин",
        }
    }
}

#[derive(Debug, Clone)]
struct PendingSave {
    mode: EditorMode,
    entry: TermEntry,
}

#[derive(Debug)]
pub struct TermsTabState {
    loaded_terms_file: Option<PathBuf>,
    entries: Vec<TermEntry>,
    tag_filter_values: Vec<String>,
    selected_tag_filter: String,
    search_query: String,
    editor: Option<TermEditorState>,
    pending_overwrite: Option<PendingSave>,
    pending_delete_name: Option<String>,
    info_message: Option<String>,
    error_message: Option<String>,
}

impl Default for TermsTabState {
    fn default() -> Self {
        Self {
            loaded_terms_file: None,
            entries: Vec::new(),
            tag_filter_values: vec![TAG_ALL.to_string()],
            selected_tag_filter: TAG_ALL.to_string(),
            search_query: String::new(),
            editor: None,
            pending_overwrite: None,
            pending_delete_name: None,
            info_message: None,
            error_message: None,
        }
    }
}

impl TermsTabState {
    pub fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, project: &ProjectData) -> bool {
        let mut changed = false;
        self.ensure_loaded(project);

        ui.vertical(|ui| {
            ui.heading("Термины");
            if let Some(msg) = &self.info_message {
                ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
            }
            if let Some(err) = &self.error_message {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), err);
            }

            ui.horizontal_wrapped(|ui| {
                ui.label("Поиск:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .desired_width(320.0)
                        .hint_text("название, оригинал, теги или описание"),
                );

                ui.add_space(8.0);
                ui.label("Тег:");
                WheelComboBox::from_id_salt("terms_tag_filter")
                    .selected_text(self.selected_tag_filter.clone())
                    .show_ui(ui, |ui| {
                        for tag in &self.tag_filter_values {
                            ui.selectable_value(&mut self.selected_tag_filter, tag.clone(), tag);
                        }
                    });

                if ui.button("Сбросить").clicked() {
                    self.search_query.clear();
                    self.selected_tag_filter = TAG_ALL.to_string();
                }

                ui.add_space(10.0);
                if ui.button("Добавить").clicked() {
                    self.error_message = None;
                    self.info_message = None;
                    self.editor = Some(TermEditorState::for_add(&self.tag_filter_values[1..]));
                }
            });

            ui.separator();

            let filtered = self.filtered_indices();
            if filtered.is_empty() {
                ui.label("Ничего не найдено.");
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for idx in filtered {
                            let entry = self.entries[idx].clone();
                            self.draw_term_card(ui, &entry);
                            ui.add_space(8.0);
                        }
                    });
            }
        });

        changed |= self.draw_editor_window(ctx, project);
        changed |= self.draw_overwrite_confirm_window(ctx, project);
        changed |= self.draw_delete_confirm_window(ctx, project);
        changed
    }

    fn draw_term_card(&mut self, ui: &mut egui::Ui, entry: &TermEntry) {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&entry.name).strong().size(18.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Удалить").clicked() {
                            self.pending_delete_name = Some(entry.name.clone());
                        }
                        if ui.button("Редактировать").clicked() {
                            self.editor = Some(TermEditorState::for_edit(
                                entry,
                                &self.tag_filter_values[1..],
                            ));
                            self.error_message = None;
                            self.info_message = None;
                        }
                    });
                });

                let orig = if entry.orig_name.trim().is_empty() {
                    "—"
                } else {
                    entry.orig_name.trim()
                };
                ui.label(
                    egui::RichText::new(format!("Оригинальное название: {orig}"))
                        .italics()
                        .color(egui::Color32::GRAY),
                );
                if !entry.tags.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("Теги: {}", entry.tags.join(", ")))
                            .italics()
                            .color(egui::Color32::GRAY),
                    );
                }
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(entry.description.clone())
                        .wrap()
                        .selectable(false),
                );
            });
    }

    fn draw_editor_window(&mut self, ctx: &egui::Context, project: &ProjectData) -> bool {
        let mut save_clicked = false;
        let mut delete_clicked = false;
        let available_tags = self.tag_filter_values.clone();
        let mut changed = false;

        if let Some(editor) = self.editor.as_mut() {
            let mut keep_open = editor.open;
            egui::Window::new(editor.title())
                .id(egui::Id::new("terms_editor_window"))
                .open(&mut keep_open)
                .resizable(true)
                .default_size(egui::vec2(620.0, 580.0))
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("Название");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.name).desired_width(f32::INFINITY),
                    );

                    ui.add_space(6.0);
                    ui.label("Оригинальное название");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.orig_name)
                            .desired_width(f32::INFINITY),
                    );

                    ui.add_space(6.0);
                    ui.label("Описание");
                    ui.add(
                        egui::TextEdit::multiline(&mut editor.description)
                            .desired_width(f32::INFINITY)
                            .desired_rows(10),
                    );

                    ui.add_space(10.0);
                    ui.label("Теги");
                    ui.horizontal(|ui| {
                        WheelComboBox::from_id_salt("terms_editor_tag_combo")
                            .selected_text(if editor.tag_input.is_empty() {
                                "выбрать/ввести".to_string()
                            } else {
                                editor.tag_input.clone()
                            })
                            .show_ui(ui, |ui| {
                                for tag in available_tags.iter().skip(1) {
                                    ui.selectable_value(&mut editor.tag_input, tag.clone(), tag);
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.tag_input)
                                .hint_text("новый тег")
                                .desired_width(180.0),
                        );
                        if ui.button("Добавить").clicked() {
                            let value = editor.tag_input.trim();
                            if !value.is_empty()
                                && !editor
                                    .tags
                                    .iter()
                                    .any(|existing| existing.to_lowercase() == value.to_lowercase())
                            {
                                editor.tags.push(value.to_string());
                                editor.tags = normalize_tags(editor.tags.clone());
                            }
                            editor.tag_input.clear();
                        }
                    });
                    ui.add_space(4.0);
                    if editor.tags.is_empty() {
                        ui.label(egui::RichText::new("Теги не выбраны").italics());
                    } else {
                        let mut remove_idx = None;
                        ui.horizontal_wrapped(|ui| {
                            for (idx, tag) in editor.tags.iter().enumerate() {
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(tag);
                                        if ui.small_button("x").clicked() {
                                            remove_idx = Some(idx);
                                        }
                                    });
                                });
                            }
                        });
                        if let Some(idx) = remove_idx {
                            editor.tags.remove(idx);
                        }
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        if matches!(editor.mode, EditorMode::Edit { .. })
                            && ui.button("Удалить").clicked()
                        {
                            delete_clicked = true;
                        }
                        ui.add_space(8.0);
                        if ui.button("Отмена").clicked() {
                            editor.open = false;
                        }
                        if ui.button("Сохранить").clicked() {
                            save_clicked = true;
                        }
                    });
                });
            editor.open = keep_open;
        }

        if save_clicked {
            changed |= self.start_editor_save(project);
        }
        if delete_clicked {
            self.start_editor_delete();
        }
        if self
            .editor
            .as_ref()
            .map(|editor| !editor.open)
            .unwrap_or(false)
        {
            self.editor = None;
        }
        changed
    }

    fn draw_overwrite_confirm_window(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) -> bool {
        let mut changed = false;
        if let Some(pending) = self.pending_overwrite.clone() {
            let mut keep_open = true;
            egui::Window::new("Подтверждение перезаписи")
                .id(egui::Id::new("terms_overwrite_confirm"))
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "«{}» уже существует. Перезаписать?",
                        pending.entry.name
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Да").clicked() {
                            self.pending_overwrite = None;
                            changed |= self.apply_pending_save(project, pending.clone());
                        }
                        if ui.button("Нет").clicked() {
                            self.pending_overwrite = None;
                        }
                    });
                });
            if !keep_open {
                self.pending_overwrite = None;
            }
        }
        changed
    }

    fn draw_delete_confirm_window(&mut self, ctx: &egui::Context, project: &ProjectData) -> bool {
        let mut changed = false;
        if let Some(name) = self.pending_delete_name.clone() {
            let mut keep_open = true;
            egui::Window::new("Удалить термин")
                .id(egui::Id::new("terms_delete_confirm"))
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Точно удалить «{name}»? Это действие нельзя отменить."
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Удалить").clicked() {
                            self.pending_delete_name = None;
                            changed |= self.delete_term(project, &name);
                        }
                        if ui.button("Отмена").clicked() {
                            self.pending_delete_name = None;
                        }
                    });
                });
            if !keep_open {
                self.pending_delete_name = None;
            }
        }
        changed
    }

    fn start_editor_save(&mut self, project: &ProjectData) -> bool {
        let Some(editor) = self.editor.as_ref().cloned() else {
            return false;
        };
        self.error_message = None;
        self.info_message = None;

        let name = safe_name(&editor.name);
        if name.trim().is_empty() {
            self.error_message = Some("Название не может быть пустым.".to_string());
            return false;
        }

        let pending = PendingSave {
            mode: editor.mode.clone(),
            entry: TermEntry {
                name,
                orig_name: editor.orig_name.trim().to_string(),
                description: editor.description.trim().to_string(),
                tags: normalize_tags(editor.tags),
            },
        };

        let needs_confirm = match &pending.mode {
            EditorMode::Add => self.find_entry_index(&pending.entry.name).is_some(),
            EditorMode::Edit { original_name } => {
                original_name != &pending.entry.name
                    && self.find_entry_index(&pending.entry.name).is_some()
            }
        };
        if needs_confirm {
            self.pending_overwrite = Some(pending);
            return false;
        }
        self.apply_pending_save(project, pending)
    }

    fn start_editor_delete(&mut self) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let name = match &editor.mode {
            EditorMode::Add => safe_name(&editor.name),
            EditorMode::Edit { original_name } => original_name.clone(),
        };
        if !name.trim().is_empty() {
            self.pending_delete_name = Some(name);
        }
    }

    fn apply_pending_save(&mut self, project: &ProjectData, pending: PendingSave) -> bool {
        match pending.mode {
            EditorMode::Add => {
                if let Some(idx) = self.find_entry_index(&pending.entry.name) {
                    self.entries[idx] = pending.entry.clone();
                } else {
                    self.entries.push(pending.entry.clone());
                }
            }
            EditorMode::Edit { original_name } => {
                if let Some(idx) = self.find_entry_index(&original_name) {
                    self.entries[idx] = pending.entry.clone();
                } else if let Some(idx) = self.find_entry_index(&pending.entry.name) {
                    self.entries[idx] = pending.entry.clone();
                } else {
                    self.entries.push(pending.entry.clone());
                }
            }
        }
        dedupe_and_sort_entries(&mut self.entries);

        if let Err(err) = save_entries(project, &self.entries) {
            self.error_message = Some(format!("Не удалось сохранить terms.json: {err}"));
            return false;
        }

        self.rebuild_tag_filters();
        self.editor = None;
        self.info_message = Some("Термин сохранён.".to_string());
        true
    }

    fn delete_term(&mut self, project: &ProjectData, name: &str) -> bool {
        let Some(idx) = self.find_entry_index(name) else {
            self.error_message = Some("Термин уже удалён.".to_string());
            return false;
        };
        self.entries.remove(idx);
        dedupe_and_sort_entries(&mut self.entries);
        if let Err(err) = save_entries(project, &self.entries) {
            self.error_message = Some(format!("Не удалось сохранить terms.json: {err}"));
            return false;
        }
        self.rebuild_tag_filters();
        self.editor = None;
        self.info_message = Some("Термин удалён.".to_string());
        true
    }

    fn ensure_loaded(&mut self, project: &ProjectData) {
        let path = project.paths.terms_file.clone();
        let needs_reload = self
            .loaded_terms_file
            .as_ref()
            .map(|loaded| loaded != &path)
            .unwrap_or(true);
        if !needs_reload {
            return;
        }
        self.loaded_terms_file = Some(path);
        self.search_query.clear();
        self.selected_tag_filter = TAG_ALL.to_string();
        self.error_message = None;
        self.info_message = None;
        self.editor = None;
        self.pending_overwrite = None;
        self.pending_delete_name = None;

        match load_entries(project) {
            Ok(entries) => {
                self.entries = entries;
                self.rebuild_tag_filters();
            }
            Err(err) => {
                self.entries.clear();
                self.rebuild_tag_filters();
                self.error_message = Some(format!("Не удалось загрузить термины: {err}"));
            }
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let term = self.search_query.trim().to_lowercase();
        let selected_tag = self.selected_tag_filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let haystack = format!(
                    "{} {} {} {}",
                    entry.name,
                    entry.orig_name,
                    entry.description,
                    entry.tags.join(" ")
                )
                .to_lowercase();
                let by_term = term.is_empty() || haystack.contains(&term);
                let by_tag = selected_tag == TAG_ALL
                    || entry
                        .tags
                        .iter()
                        .any(|tag| tag.trim().to_lowercase() == selected_tag);
                if by_term && by_tag { Some(idx) } else { None }
            })
            .collect()
    }

    fn rebuild_tag_filters(&mut self) {
        let mut tags: HashSet<String> = HashSet::new();
        for entry in &self.entries {
            for tag in &entry.tags {
                let trimmed = tag.trim();
                if !trimmed.is_empty() {
                    tags.insert(trimmed.to_string());
                }
            }
        }
        let mut values: Vec<String> = tags.into_iter().collect();
        values.sort_by_key(|v| v.to_lowercase());
        values.insert(0, TAG_ALL.to_string());

        if !values.contains(&self.selected_tag_filter) {
            self.selected_tag_filter = TAG_ALL.to_string();
        }
        self.tag_filter_values = values;
    }

    fn find_entry_index(&self, name: &str) -> Option<usize> {
        let key = name.to_lowercase();
        self.entries
            .iter()
            .position(|entry| entry.name.to_lowercase() == key)
    }
}

pub fn load_terms_for_notes(project: &ProjectData) -> Result<Vec<TermNoteEntry>, String> {
    let entries = load_entries(project)?;
    Ok(entries
        .into_iter()
        .map(|entry| TermNoteEntry {
            name: entry.name,
            orig_name: entry.orig_name,
            description: entry.description,
            tags: entry.tags,
        })
        .collect())
}

fn load_entries(project: &ProjectData) -> Result<Vec<TermEntry>, String> {
    let path = terms_path_for(project);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let parsed: Vec<TermEntry> = serde_json::from_str(&raw).map_err(|err| err.to_string())?;

    let mut normalized = parsed
        .into_iter()
        .filter_map(|entry| {
            let name = safe_name(&entry.name);
            if name.trim().is_empty() {
                return None;
            }
            Some(TermEntry {
                name,
                orig_name: entry.orig_name.trim().to_string(),
                description: entry.description,
                tags: normalize_tags(entry.tags),
            })
        })
        .collect::<Vec<_>>();
    dedupe_and_sort_entries(&mut normalized);
    Ok(normalized)
}

fn save_entries(project: &ProjectData, entries: &[TermEntry]) -> Result<(), String> {
    let path = terms_path_for(project);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(entries).map_err(|err| err.to_string())?;
    fs::write(&tmp, raw).map_err(|err| err.to_string())?;
    if path.exists() {
        fs::remove_file(path).map_err(|err| err.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|err| err.to_string())?;
    Ok(())
}

fn dedupe_and_sort_entries(entries: &mut Vec<TermEntry>) {
    let mut map = std::collections::HashMap::new();
    for entry in entries.drain(..) {
        map.insert(entry.name.to_lowercase(), entry);
    }
    let mut values: Vec<TermEntry> = map.into_values().collect();
    values.sort_by_key(|entry| entry.name.to_lowercase());
    *entries = values;
}

fn normalize_tags(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out.sort_by_key(|value| value.to_lowercase());
    out
}

fn safe_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
        if out.chars().count() >= MAX_NAME_LEN {
            break;
        }
    }
    out
}

fn terms_path_for(project: &ProjectData) -> &Path {
    &project.paths.terms_file
}
