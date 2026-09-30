/*
FILE OVERVIEW: crates/ms-tabs-simple/src/terms.rs
Terms tab state and CRUD UI for project-scoped `terms.json`.

Main items:
- `TermsTabState`: cached list/filter/editor state, with lazy reload per active project.
- `TermEntry`: persisted term schema (`name`, `orig_name`, `description`, `tags`).
- Editor/confirm windows: add/edit/delete flows and overwrite confirmation.

Storage behavior:
- Reads/writes `project.paths.terms_file` ONLY through `ms_docstore` (`read` / atomic
  `write`); load/save still run on the GUI thread (pre-existing CLAUDE.md §5 gap), so saves
  are not fsynced (`Durability::None`). After a failed load every save is refused
  (`load_error`), and a save never replaces a malformed existing document.
- Supports legacy `tags` wire format as string or string array.
- Keeps names/tags normalized and sorted, with case-insensitive dedupe.
*/

use ms_project::ProjectData;
use ms_widgets::WheelComboBox;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Label for the "all tags" filter option. It is both a display label and the `==`
/// sentinel meaning "no tag filter". Runtime (not `const`) because `t!` is not const.
/// The filter selection is session-only (never persisted), so a live UI-language switch
/// merely re-seeds it on the next term reload.
fn tag_all() -> &'static str {
    t!("terms.list.tag_filter_all")
}
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
            EditorMode::Add => t!("terms.editor.title_add"),
            EditorMode::Edit { .. } => t!("terms.editor.title_edit"),
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
    /// The localized load failure of the current glossary, while it lasts. `entries` is
    /// then EMPTY (not the document's content), so every save and delete is refused and
    /// this message shown again: writing the empty list back would replace the unreadable
    /// or malformed file. Cleared only by a successful (re)load.
    load_error: Option<String>,
}

impl Default for TermsTabState {
    fn default() -> Self {
        Self {
            loaded_terms_file: None,
            entries: Vec::new(),
            tag_filter_values: vec![tag_all().to_string()],
            selected_tag_filter: tag_all().to_string(),
            search_query: String::new(),
            editor: None,
            pending_overwrite: None,
            pending_delete_name: None,
            info_message: None,
            error_message: None,
            load_error: None,
        }
    }
}

impl TermsTabState {
    pub fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, project: &ProjectData) -> bool {
        let mut changed = false;
        self.ensure_loaded(project);

        ui.vertical(|ui| {
            ui.heading(t!("terms.list.heading"));
            if let Some(msg) = &self.info_message {
                ui.colored_label(ms_theme::status::SUCCESS, msg);
            }
            if let Some(err) = &self.error_message {
                ui.colored_label(ms_theme::status::ERROR, err);
            }

            ui.horizontal_wrapped(|ui| {
                ui.label(t!("terms.list.search_label"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .desired_width(320.0)
                        .hint_text(t!("terms.list.search_placeholder")),
                );

                ui.add_space(8.0);
                ui.label(t!("terms.list.tag_label"));
                WheelComboBox::from_id_salt("terms_tag_filter")
                    .selected_text(self.selected_tag_filter.clone())
                    .show_ui(ui, |ui| {
                        for tag in &self.tag_filter_values {
                            ui.selectable_value(&mut self.selected_tag_filter, tag.clone(), tag);
                        }
                    });

                if ui.button(t!("terms.list.reset_filter_button")).clicked() {
                    self.search_query.clear();
                    self.selected_tag_filter = tag_all().to_string();
                }

                ui.add_space(10.0);
                if ui.button(t!("terms.list.add_button")).clicked() {
                    self.error_message = None;
                    self.info_message = None;
                    self.editor = Some(TermEditorState::for_add(&self.tag_filter_values[1..]));
                }
            });

            ui.separator();

            let filtered = self.filtered_indices();
            if filtered.is_empty() {
                ui.label(t!("terms.list.empty"));
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
                        if ui.button(t!("terms.card.delete_button")).clicked() {
                            self.pending_delete_name = Some(entry.name.clone());
                        }
                        if ui.button(t!("terms.card.edit_button")).clicked() {
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
                    egui::RichText::new(tf!("terms.card.orig_name", orig = orig))
                        .italics()
                        .color(ui.visuals().weak_text_color()),
                );
                if !entry.tags.is_empty() {
                    ui.label(
                        egui::RichText::new(tf!("terms.card.tags", tags = entry.tags.join(", ")))
                            .italics()
                            .color(ui.visuals().weak_text_color()),
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
                    ui.label(t!("terms.editor.name_label"));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.name).desired_width(f32::INFINITY),
                    );

                    ui.add_space(6.0);
                    ui.label(t!("terms.editor.orig_name_label"));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.orig_name)
                            .desired_width(f32::INFINITY),
                    );

                    ui.add_space(6.0);
                    ui.label(t!("terms.editor.description_label"));
                    ui.add(
                        egui::TextEdit::multiline(&mut editor.description)
                            .desired_width(f32::INFINITY)
                            .desired_rows(10),
                    );

                    ui.add_space(10.0);
                    ui.label(t!("terms.editor.tags_label"));
                    ui.horizontal(|ui| {
                        WheelComboBox::from_id_salt("terms_editor_tag_combo")
                            .selected_text(if editor.tag_input.is_empty() {
                                t!("terms.editor.tags_combo_placeholder").to_string()
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
                                .hint_text(t!("terms.editor.new_tag_placeholder"))
                                .desired_width(180.0),
                        );
                        if ui.button(t!("terms.editor.add_tag_button")).clicked() {
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
                        ui.label(egui::RichText::new(t!("terms.editor.tags_empty")).italics());
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
                            && ui.button(t!("terms.editor.delete_button")).clicked()
                        {
                            delete_clicked = true;
                        }
                        ui.add_space(8.0);
                        if ui.button(t!("terms.editor.cancel_button")).clicked() {
                            editor.open = false;
                        }
                        if ui.button(t!("terms.editor.save_button")).clicked() {
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
            egui::Window::new(t!("terms.overwrite_dialog.title"))
                .id(egui::Id::new("terms_overwrite_confirm"))
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(tf!("terms.overwrite_dialog.message", name = pending.entry.name));
                    ui.horizontal(|ui| {
                        if ui.button(t!("terms.overwrite_dialog.yes_button")).clicked() {
                            self.pending_overwrite = None;
                            changed |= self.apply_pending_save(project, pending.clone());
                        }
                        if ui.button(t!("terms.overwrite_dialog.no_button")).clicked() {
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
            egui::Window::new(t!("terms.delete_dialog.title"))
                .id(egui::Id::new("terms_delete_confirm"))
                .open(&mut keep_open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(tf!("terms.delete_dialog.message", name = name));
                    ui.horizontal(|ui| {
                        if ui.button(t!("terms.delete_dialog.confirm_button")).clicked() {
                            self.pending_delete_name = None;
                            changed |= self.delete_term(project, &name);
                        }
                        if ui.button(t!("terms.delete_dialog.cancel_button")).clicked() {
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
            self.error_message = Some(t!("terms.editor.name_empty_error").to_string());
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
        if !self.writes_allowed() {
            return false;
        }
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
            self.error_message = Some(tf!("terms.save.save_error", err = err));
            return false;
        }

        self.rebuild_tag_filters();
        self.editor = None;
        self.info_message = Some(t!("terms.save.saved").to_string());
        true
    }

    fn delete_term(&mut self, project: &ProjectData, name: &str) -> bool {
        if !self.writes_allowed() {
            return false;
        }
        let Some(idx) = self.find_entry_index(name) else {
            self.error_message = Some(t!("terms.save.already_deleted").to_string());
            return false;
        };
        self.entries.remove(idx);
        dedupe_and_sort_entries(&mut self.entries);
        if let Err(err) = save_entries(project, &self.entries) {
            self.error_message = Some(tf!("terms.save.save_error", err = err));
            return false;
        }
        self.rebuild_tag_filters();
        self.editor = None;
        self.info_message = Some(t!("terms.save.deleted").to_string());
        true
    }

    /// Whether the in-memory glossary may be written back. After a failed load it may not
    /// (see `load_error`); the load error is then shown again instead.
    fn writes_allowed(&mut self) -> bool {
        match &self.load_error {
            Some(load_error) => {
                self.error_message = Some(load_error.clone());
                false
            }
            None => true,
        }
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
        self.selected_tag_filter = tag_all().to_string();
        self.error_message = None;
        self.info_message = None;
        self.editor = None;
        self.pending_overwrite = None;
        self.pending_delete_name = None;

        match load_entries(project) {
            Ok(entries) => {
                self.entries = entries;
                self.load_error = None;
                self.rebuild_tag_filters();
            }
            Err(err) => {
                self.entries.clear();
                self.rebuild_tag_filters();
                let message = tf!("terms.save.load_error", err = err);
                self.error_message = Some(message.clone());
                self.load_error = Some(message);
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
                let by_tag = selected_tag == tag_all()
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
        values.insert(0, tag_all().to_string());

        if !values.contains(&self.selected_tag_filter) {
            self.selected_tag_filter = tag_all().to_string();
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

/// Loads the title's glossary (`terms.json`), normalized (safe non-empty names, trimmed
/// original names, normalized tags, deduped and sorted by lowercase name). An absent
/// document is an empty glossary.
///
/// # Errors
/// Returns a technical message when the document cannot be read or does not parse as a
/// glossary; the file is left untouched.
fn load_entries(project: &ProjectData) -> Result<Vec<TermEntry>, String> {
    load_entries_from(terms_path_for(project))
}

/// [`load_entries`] for the glossary file `terms_file`.
///
/// # Errors
/// As [`load_entries`].
fn load_entries_from(terms_file: &Path) -> Result<Vec<TermEntry>, String> {
    let Some(parsed) = ms_docstore::read::<Vec<TermEntry>>(&terms_doc(terms_file)).map_err(|err| err.to_string())? else {
        return Ok(Vec::new());
    };

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

/// Replaces the title's `terms.json` with `entries` (atomic write through the document
/// store, historical 2-space layout without a trailing newline; the parent directory is
/// created when missing). Runs on the GUI thread, so the write is not fsynced
/// (`Durability::None`: a crash may lose it, never tear the file).
///
/// # Errors
/// Returns a technical message when the existing document cannot be read or is malformed
/// (it is never replaced), or the new one cannot be serialized or written.
fn save_entries(project: &ProjectData, entries: &[TermEntry]) -> Result<(), String> {
    save_entries_to(terms_path_for(project), entries, ms_docstore::Durability::None)
}

/// [`save_entries`] for the glossary file `terms_file` with `durability`. Under the
/// document lock the existing document is read first: an unreadable or malformed one is
/// NEVER replaced (the check and the write are one critical section).
///
/// # Errors
/// As [`save_entries`].
fn save_entries_to(terms_file: &Path, entries: &[TermEntry], durability: ms_docstore::Durability) -> Result<(), String> {
    let options = ms_docstore::WriteOptions { durability, ..ms_docstore::WriteOptions::default() };
    ms_docstore::with_lock(&terms_doc(terms_file), |locked| {
        locked.read_value().map_err(|err| err.to_string())?;
        locked.write(entries, options).map(|_| ()).map_err(|err| err.to_string())
    })
}

/// The glossary file `terms_file` as a docstore document.
fn terms_doc(terms_file: &Path) -> ms_docstore::DocRef {
    ms_docstore::DocRef::new(terms_file, ms_docstore::DocKind::Terms)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod persistence_tests {
    use super::{TermEntry, TermsTabState, load_entries_from, save_entries_to};
    use ms_docstore::Durability;

    fn io<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
        result.map_err(|err| err.to_string())
    }

    /// Absent glossary is empty; a malformed one is an error and stays byte-identical.
    #[test]
    fn absent_is_empty_and_malformed_is_left_untouched() -> Result<(), String> {
        let dir = io(tempfile::tempdir())?;
        let path = dir.path().join("terms.json");
        assert!(load_entries_from(&path)?.is_empty());
        io(std::fs::write(&path, "{oops"))?;
        assert!(load_entries_from(&path).is_err());
        assert_eq!(io(std::fs::read_to_string(&path))?, "{oops");
        Ok(())
    }

    /// A save never replaces a malformed glossary (e.g. corrupted after it was loaded).
    #[test]
    fn save_refuses_to_replace_a_malformed_glossary() -> Result<(), String> {
        let dir = io(tempfile::tempdir())?;
        let path = dir.path().join("terms.json");
        io(std::fs::write(&path, "{oops"))?;
        let entries = vec![TermEntry { name: "Qi".to_string(), orig_name: String::new(), description: String::new(), tags: Vec::new() }];
        assert!(save_entries_to(&path, &entries, Durability::None).is_err());
        assert_eq!(io(std::fs::read_to_string(&path))?, "{oops");
        Ok(())
    }

    /// After a failed load the tab refuses to write and shows the load error again.
    #[test]
    fn a_failed_load_blocks_writes_until_a_successful_reload() {
        let mut state = TermsTabState { load_error: Some("load failed".to_string()), ..TermsTabState::default() };
        assert!(!state.writes_allowed());
        assert_eq!(state.error_message.as_deref(), Some("load failed"));
        state.load_error = None;
        assert!(state.writes_allowed());
    }

    /// A saved glossary keeps the historical layout (struct field order, no trailing
    /// newline) and reads back.
    #[test]
    fn save_round_trips_in_historical_layout() -> Result<(), String> {
        let dir = io(tempfile::tempdir())?;
        let path = dir.path().join("terms.json");
        let entries = vec![TermEntry { name: "Qi".to_string(), orig_name: "气".to_string(), description: "energy".to_string(), tags: vec!["x".to_string()] }];
        save_entries_to(&path, &entries, Durability::None)?;
        assert_eq!(io(std::fs::read_to_string(&path))?, io(serde_json::to_string_pretty(&entries))?);
        let loaded = load_entries_from(&path)?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].orig_name, "气");
        Ok(())
    }
}
