use crate::paste_image;
use crate::project::ProjectData;
use crate::widgets::WheelComboBox;
use eframe::egui;
use egui::{ColorImage, TextureHandle, TextureOptions};
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread::{self as thread, JoinHandle};

/// Label for the "all groups" filter option. It is both a display label and the `==`
/// sentinel meaning "no group filter". Runtime (not `const`) because `t!` is not const.
/// The filter selection is session-only (never persisted), so a live UI-language switch
/// merely re-seeds it on the next character reload.
fn group_all() -> &'static str {
    t!("characters.list.group_filter_all")
}
const CARD_THUMB_SIDE_PX: u32 = 192;
const EDITOR_PREVIEW_SIDE_PX: f32 = 320.0;
const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone)]
pub enum CharactersTabAction {
    CharactersChanged,
    OpenNotesForCharacter(String),
}

#[derive(Debug, Clone)]
pub struct CharacterNoteEntry {
    pub name: String,
    pub description: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CharacterEntry {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "group", default, deserialize_with = "deserialize_groups")]
    groups: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum GroupsWire {
    One(String),
    Many(Vec<String>),
}

fn deserialize_groups<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let parsed = Option::<GroupsWire>::deserialize(deserializer)?;
    Ok(match parsed {
        Some(GroupsWire::One(v)) => vec![v],
        Some(GroupsWire::Many(v)) => v,
        None => Vec::new(),
    })
}

#[derive(Debug, Clone)]
enum EditorMode {
    Add,
    Edit { original_name: String },
}

#[derive(Clone)]
struct CharacterEditorState {
    mode: EditorMode,
    name: String,
    description: String,
    groups: Vec<String>,
    group_input: String,
    image_path_input: String,
    pending_image: Option<ColorImage>,
    pending_texture: Option<TextureHandle>,
    pending_image_rev: u64,
    texture_rev: u64,
    remove_image: bool,
    open_notes_after_save: bool,
    open: bool,
}

impl CharacterEditorState {
    fn for_add(available_groups: &[String]) -> Self {
        Self {
            mode: EditorMode::Add,
            name: String::new(),
            description: String::new(),
            groups: Vec::new(),
            group_input: available_groups.first().cloned().unwrap_or_default(),
            image_path_input: String::new(),
            pending_image: None,
            pending_texture: None,
            pending_image_rev: 0,
            texture_rev: 0,
            remove_image: false,
            open_notes_after_save: false,
            open: true,
        }
    }

    fn for_edit(entry: &CharacterEntry, image_path: &Path, available_groups: &[String]) -> Self {
        Self {
            mode: EditorMode::Edit {
                original_name: entry.name.clone(),
            },
            name: entry.name.clone(),
            description: entry.description.clone(),
            groups: entry.groups.clone(),
            group_input: available_groups.first().cloned().unwrap_or_default(),
            image_path_input: image_path.display().to_string(),
            pending_image: None,
            pending_texture: None,
            pending_image_rev: 0,
            texture_rev: 0,
            remove_image: false,
            open_notes_after_save: false,
            open: true,
        }
    }

    fn title(&self) -> &'static str {
        match self.mode {
            EditorMode::Add => t!("characters.editor.title_add"),
            EditorMode::Edit { .. } => t!("characters.editor.title_edit"),
        }
    }
}

#[derive(Clone)]
struct PendingSave {
    mode: EditorMode,
    entry: CharacterEntry,
    pending_image: Option<ColorImage>,
    remove_image: bool,
    open_notes_after_save: bool,
}

#[derive(Debug)]
enum ThumbnailJob {
    Load { name: String, path: PathBuf },
    Stop,
}

#[derive(Debug)]
struct ThumbnailResult {
    name: String,
    decoded: Option<DecodedImage>,
    missing: bool,
    error: Option<String>,
}

#[derive(Debug)]
struct ClipboardImageResult {
    decoded: Option<DecodedImage>,
    error: Option<String>,
}

#[derive(Debug)]
struct DecodedImage {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

enum ThumbnailState {
    Loading,
    Ready(TextureHandle),
    Missing,
    Failed,
}

pub struct CharactersTabState {
    loaded_chars_dir: Option<PathBuf>,
    entries: Vec<CharacterEntry>,
    groups_filter_values: Vec<String>,
    selected_group_filter: String,
    search_query: String,
    editor: Option<CharacterEditorState>,
    pending_overwrite: Option<PendingSave>,
    pending_delete_name: Option<String>,
    info_message: Option<String>,
    error_message: Option<String>,
    thumb_tx: Sender<ThumbnailJob>,
    thumb_rx: Receiver<ThumbnailResult>,
    thumb_worker: Option<JoinHandle<()>>,
    thumb_state: HashMap<String, ThumbnailState>,
    thumb_requested: HashSet<String>,
    thumb_texture_serial: u64,
    clipboard_rx: Option<Receiver<ClipboardImageResult>>,
    clipboard_in_flight: bool,
}

impl Default for CharactersTabState {
    fn default() -> Self {
        let (thumb_tx, thumb_rx, thumb_worker) = spawn_thumbnail_worker();
        Self {
            loaded_chars_dir: None,
            entries: Vec::new(),
            groups_filter_values: vec![group_all().to_string()],
            selected_group_filter: group_all().to_string(),
            search_query: String::new(),
            editor: None,
            pending_overwrite: None,
            pending_delete_name: None,
            info_message: None,
            error_message: None,
            thumb_tx,
            thumb_rx,
            thumb_worker: Some(thumb_worker),
            thumb_state: HashMap::new(),
            thumb_requested: HashSet::new(),
            thumb_texture_serial: 0,
            clipboard_rx: None,
            clipboard_in_flight: false,
        }
    }
}

impl Drop for CharactersTabState {
    fn drop(&mut self) {
        let _ = self.thumb_tx.send(ThumbnailJob::Stop);
        if let Some(handle) = self.thumb_worker.take() {
            let _ = handle.join();
        }
    }
}

pub fn load_character_names(project: &ProjectData) -> Result<Vec<String>, String> {
    let entries = load_entries(project)?;
    Ok(entries.into_iter().map(|entry| entry.name).collect())
}

pub fn load_characters_for_notes(project: &ProjectData) -> Result<Vec<CharacterNoteEntry>, String> {
    let entries = load_entries(project)?;
    Ok(entries
        .into_iter()
        .map(|entry| CharacterNoteEntry {
            name: entry.name,
            description: entry.description,
            groups: entry.groups,
        })
        .collect())
}

impl CharactersTabState {
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        project: &ProjectData,
    ) -> Vec<CharactersTabAction> {
        self.ensure_loaded(project);
        self.poll_thumbnail_results(ctx);
        self.poll_clipboard_result();
        if self.clipboard_in_flight {
            ctx.request_repaint();
        }

        let mut actions = Vec::new();
        ui.vertical(|ui| {
            ui.heading(t!("characters.list.heading"));
            if let Some(msg) = &self.info_message {
                ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
            }
            if let Some(err) = &self.error_message {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), err);
            }

            ui.horizontal_wrapped(|ui| {
                ui.label(t!("characters.list.search_label"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.search_query)
                        .desired_width(320.0)
                        .hint_text(t!("characters.list.search_placeholder")),
                );

                ui.add_space(8.0);
                ui.label(t!("characters.list.group_label"));
                WheelComboBox::from_id_salt("characters_group_filter")
                    .selected_text(self.selected_group_filter.clone())
                    .show_ui(ui, |ui| {
                        for group in &self.groups_filter_values {
                            ui.selectable_value(
                                &mut self.selected_group_filter,
                                group.clone(),
                                group,
                            );
                        }
                    });

                if ui.button(t!("characters.list.reset_filter_button")).clicked() {
                    self.search_query.clear();
                    self.selected_group_filter = group_all().to_string();
                }

                ui.add_space(10.0);
                if ui.button(t!("characters.list.add_button")).clicked() {
                    self.error_message = None;
                    self.info_message = None;
                    self.editor = Some(CharacterEditorState::for_add(
                        &self.groups_filter_values[1..],
                    ));
                }
            });

            ui.separator();

            let filtered = self.filtered_indices();
            if filtered.is_empty() {
                ui.label(t!("characters.list.empty"));
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for idx in filtered {
                            let entry = self.entries[idx].clone();
                            let card_action = self.draw_character_card(ui, project, &entry);
                            if let Some(action) = card_action {
                                actions.push(action);
                            }
                            ui.add_space(8.0);
                        }
                    });
            }
        });

        if let Some(action) = self.draw_editor_window(ctx, project) {
            actions.push(action);
        }
        if let Some(action) = self.draw_overwrite_confirm_window(ctx, project) {
            actions.push(action);
        }
        if let Some(action) = self.draw_delete_confirm_window(ctx, project) {
            actions.push(action);
        }

        actions
    }

    fn draw_character_card(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        entry: &CharacterEntry,
    ) -> Option<CharactersTabAction> {
        let image_path = image_path_for(project, &entry.name);
        self.ensure_thumbnail_requested(&entry.name, image_path);

        let mut action = None;
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        let thumb_size =
                            egui::vec2(CARD_THUMB_SIDE_PX as f32, CARD_THUMB_SIDE_PX as f32);
                        match self.thumb_state.get(&entry.name) {
                            Some(ThumbnailState::Ready(tex)) => {
                                ui.add(
                                    egui::Image::new((tex.id(), thumb_size))
                                        .corner_radius(egui::CornerRadius::same(6)),
                                );
                            }
                            Some(ThumbnailState::Loading) => {
                                ui.allocate_ui_with_layout(
                                    thumb_size,
                                    egui::Layout::top_down_justified(egui::Align::Center),
                                    |ui| {
                                        ui.centered_and_justified(|ui| {
                                            ui.label(t!("characters.card.thumbnail_loading"));
                                        });
                                    },
                                );
                            }
                            Some(ThumbnailState::Missing) => {
                                ui.allocate_ui_with_layout(
                                    thumb_size,
                                    egui::Layout::top_down_justified(egui::Align::Center),
                                    |ui| {
                                        ui.centered_and_justified(|ui| {
                                            ui.label(t!("characters.card.thumbnail_none"));
                                        });
                                    },
                                );
                            }
                            Some(ThumbnailState::Failed) | None => {
                                ui.allocate_ui_with_layout(
                                    thumb_size,
                                    egui::Layout::top_down_justified(egui::Align::Center),
                                    |ui| {
                                        ui.centered_and_justified(|ui| {
                                            ui.label(t!("characters.card.thumbnail_error"));
                                        });
                                    },
                                );
                            }
                        }

                        if ui.button(t!("characters.card.edit_button")).clicked() {
                            self.editor = Some(CharacterEditorState::for_edit(
                                entry,
                                &image_path_for(project, &entry.name),
                                &self.groups_filter_values[1..],
                            ));
                            self.error_message = None;
                            self.info_message = None;
                        }
                        if ui.button(t!("characters.card.to_notes_button")).clicked() {
                            action = Some(CharactersTabAction::OpenNotesForCharacter(
                                entry.name.clone(),
                            ));
                        }
                    });

                    ui.add_space(10.0);

                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&entry.name).strong().size(18.0));
                        if !entry.groups.is_empty() {
                            ui.label(
                                egui::RichText::new(tf!("characters.card.groups", groups = entry.groups.join(", ")))
                                    .italics()
                                    .color(egui::Color32::GRAY),
                            );
                        }
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new(entry.description.clone())
                                .wrap()
                                .selectable(false),
                        );
                    });
                });
            });
        action
    }

    fn draw_editor_window(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) -> Option<CharactersTabAction> {
        let mut action = None;
        let mut save_clicked = false;
        let mut delete_clicked = false;
        let available_groups = self.groups_filter_values.clone();
        let mut load_image_error: Option<String> = None;
        let mut clear_error = false;
        let clipboard_busy = self.clipboard_in_flight;
        let mut request_clipboard_paste = false;

        if let Some(editor) = self.editor.as_mut() {
            let mut keep_open = editor.open;
            egui::Window::new(editor.title())
                .id(egui::Id::new("characters_editor_window"))
                .open(&mut keep_open)
                .resizable(true)
                .default_size(egui::vec2(560.0, 700.0))
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(t!("characters.editor.name_label"));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.name).desired_width(f32::INFINITY),
                    );

                    ui.add_space(6.0);
                    ui.label(t!("characters.editor.description_label"));
                    ui.add(
                        egui::TextEdit::multiline(&mut editor.description)
                            .desired_width(f32::INFINITY)
                            .desired_rows(8),
                    );

                    ui.add_space(10.0);
                    ui.label(t!("characters.editor.groups_label"));
                    ui.horizontal(|ui| {
                        WheelComboBox::from_id_salt("characters_editor_group_combo")
                            .selected_text(if editor.group_input.is_empty() {
                                t!("characters.editor.groups_combo_placeholder").to_string()
                            } else {
                                editor.group_input.clone()
                            })
                            .show_ui(ui, |ui| {
                                for group in available_groups.iter().skip(1) {
                                    ui.selectable_value(
                                        &mut editor.group_input,
                                        group.clone(),
                                        group,
                                    );
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.group_input)
                                .hint_text(t!("characters.editor.new_group_placeholder"))
                                .desired_width(180.0),
                        );
                        if ui.button(t!("characters.editor.add_group_button")).clicked() {
                            let value = editor.group_input.trim();
                            if !value.is_empty()
                                && !editor
                                    .groups
                                    .iter()
                                    .any(|existing| existing.to_lowercase() == value.to_lowercase())
                            {
                                editor.groups.push(value.to_string());
                                editor.groups = normalize_groups(editor.groups.clone());
                            }
                            editor.group_input.clear();
                        }
                    });
                    ui.add_space(4.0);
                    if editor.groups.is_empty() {
                        ui.label(egui::RichText::new(t!("characters.editor.groups_empty")).italics());
                    } else {
                        let mut remove_idx = None;
                        ui.horizontal_wrapped(|ui| {
                            for (idx, group) in editor.groups.iter().enumerate() {
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(group);
                                        if ui.small_button("x").clicked() {
                                            remove_idx = Some(idx);
                                        }
                                    });
                                });
                            }
                        });
                        if let Some(idx) = remove_idx {
                            editor.groups.remove(idx);
                        }
                    }

                    ui.separator();
                    ui.label(t!("characters.editor.image_label"));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.image_path_input)
                                .desired_width(340.0)
                                .hint_text(t!("characters.editor.image_path_placeholder")),
                        );
                        if ui.button(t!("characters.editor.image_load_button")).clicked() {
                            match load_image_from_path(Path::new(editor.image_path_input.trim())) {
                                Ok(img) => {
                                    editor.pending_image = Some(img);
                                    editor.pending_texture = None;
                                    editor.pending_image_rev =
                                        editor.pending_image_rev.saturating_add(1);
                                    editor.remove_image = false;
                                    clear_error = true;
                                }
                                Err(err) => {
                                    load_image_error =
                                        Some(tf!("characters.editor.image_load_error", err = err));
                                }
                            }
                        }
                        if ui
                            .add_enabled(!clipboard_busy, egui::Button::new(t!("characters.editor.image_paste_button")))
                            .clicked()
                        {
                            request_clipboard_paste = true;
                        }
                    });
                    if clipboard_busy {
                        ui.small(t!("characters.editor.image_paste_reading"));
                    }

                    ui.horizontal(|ui| {
                        if ui.button(t!("characters.editor.image_reset_new_button")).clicked()
                        {
                            editor.pending_image = None;
                            editor.pending_texture = None;
                            editor.pending_image_rev = editor.pending_image_rev.saturating_add(1);
                        }
                        if matches!(editor.mode, EditorMode::Edit { .. })
                            && ui.button(t!("characters.editor.image_delete_current_button")).clicked()
                        {
                            editor.remove_image = true;
                            editor.pending_image = None;
                            editor.pending_texture = None;
                            editor.pending_image_rev = editor.pending_image_rev.saturating_add(1);
                        }
                    });

                    ui.add_space(6.0);
                    draw_editor_preview(ui, ctx, editor);
                    ui.checkbox(
                        &mut editor.open_notes_after_save,
                        t!("characters.editor.go_to_notes_after_save_label"),
                    );

                    ui.separator();
                    ui.horizontal(|ui| {
                        if matches!(editor.mode, EditorMode::Edit { .. })
                            && ui.button(t!("characters.editor.delete_button")).clicked()
                        {
                            delete_clicked = true;
                        }
                        ui.add_space(8.0);
                        if ui.button(t!("characters.editor.cancel_button")).clicked() {
                            editor.open = false;
                        }
                        if ui.button(t!("characters.editor.save_button")).clicked() {
                            save_clicked = true;
                        }
                    });
                });
            editor.open = keep_open;
        }

        if clear_error {
            self.error_message = None;
        }
        if request_clipboard_paste {
            self.start_clipboard_paste_request();
        }
        if let Some(err) = load_image_error {
            self.error_message = Some(err);
        }
        if save_clicked {
            action = self.start_editor_save(project);
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

        action
    }

    fn draw_overwrite_confirm_window(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) -> Option<CharactersTabAction> {
        let mut action = None;
        if let Some(pending) = self.pending_overwrite.clone() {
            let mut keep_window = true;
            egui::Window::new(t!("characters.overwrite_dialog.title"))
                .id(egui::Id::new("characters_overwrite_confirm"))
                .open(&mut keep_window)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(tf!("characters.overwrite_dialog.message", name = pending.entry.name));
                    ui.horizontal(|ui| {
                        if ui.button(t!("characters.overwrite_dialog.yes_button")).clicked() {
                            self.pending_overwrite = None;
                            action = self.apply_pending_save(project, pending.clone());
                        }
                        if ui.button(t!("characters.overwrite_dialog.no_button")).clicked() {
                            self.pending_overwrite = None;
                        }
                    });
                });
            if !keep_window {
                self.pending_overwrite = None;
            }
        }
        action
    }

    fn draw_delete_confirm_window(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) -> Option<CharactersTabAction> {
        let mut action = None;
        if let Some(name) = self.pending_delete_name.clone() {
            let mut keep_window = true;
            egui::Window::new(t!("characters.delete_dialog.title"))
                .id(egui::Id::new("characters_delete_confirm"))
                .open(&mut keep_window)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(tf!("characters.delete_dialog.message", name = name));
                    ui.horizontal(|ui| {
                        if ui.button(t!("characters.delete_dialog.confirm_button")).clicked() {
                            self.pending_delete_name = None;
                            action = self.delete_character(project, &name);
                        }
                        if ui.button(t!("characters.delete_dialog.cancel_button")).clicked() {
                            self.pending_delete_name = None;
                        }
                    });
                });
            if !keep_window {
                self.pending_delete_name = None;
            }
        }
        action
    }

    fn start_editor_save(&mut self, project: &ProjectData) -> Option<CharactersTabAction> {
        let editor = self.editor.as_ref().cloned()?;
        self.error_message = None;
        self.info_message = None;

        let safe = safe_name(&editor.name);
        if safe.trim().is_empty() {
            self.error_message = Some(t!("characters.editor.name_empty_error").to_string());
            return None;
        }
        let pending = PendingSave {
            mode: editor.mode.clone(),
            entry: CharacterEntry {
                name: safe,
                description: editor.description.trim().to_string(),
                groups: normalize_groups(editor.groups),
            },
            pending_image: editor.pending_image.clone(),
            remove_image: editor.remove_image,
            open_notes_after_save: editor.open_notes_after_save,
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
            return None;
        }
        self.apply_pending_save(project, pending)
    }

    fn apply_pending_save(
        &mut self,
        project: &ProjectData,
        pending: PendingSave,
    ) -> Option<CharactersTabAction> {
        let original_name = match &pending.mode {
            EditorMode::Add => None,
            EditorMode::Edit { original_name } => Some(original_name.clone()),
        };
        let new_name = pending.entry.name.clone();

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
            self.error_message = Some(tf!("characters.save.save_error", err = err));
            return None;
        }

        if let Some(old_name) = original_name {
            let store = crate::storage::storage();
            let old_path = image_path_for(project, &old_name);
            let new_path = image_path_for(project, &new_name);
            let old_path_str = old_path.to_string_lossy();
            let new_path_str = new_path.to_string_lossy();
            if old_name != new_name && store.exists(old_path_str.as_ref()) {
                if store.exists(new_path_str.as_ref()) {
                    let _ = store.remove_file(new_path_str.as_ref());
                }
                let _ = store.rename(old_path_str.as_ref(), new_path_str.as_ref());
            }
            self.clear_thumbnail(&old_name);
        }
        self.clear_thumbnail(&new_name);

        let target_image_path = image_path_for(project, &new_name);
        if pending.remove_image {
            let target_image_str = target_image_path.to_string_lossy();
            let _ = crate::storage::storage().remove_file(target_image_str.as_ref());
            self.clear_thumbnail(&new_name);
        }
        if let Some(image) = pending.pending_image {
            if let Err(err) = save_color_image_png(&target_image_path, &image) {
                self.error_message =
                    Some(tf!("characters.save.image_save_error", err = err));
            } else {
                self.clear_thumbnail(&new_name);
            }
        }

        self.rebuild_group_filters();
        self.editor = None;
        self.info_message = Some(t!("characters.save.saved").to_string());

        if pending.open_notes_after_save {
            return Some(CharactersTabAction::OpenNotesForCharacter(new_name));
        }
        Some(CharactersTabAction::CharactersChanged)
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

    fn delete_character(
        &mut self,
        project: &ProjectData,
        name: &str,
    ) -> Option<CharactersTabAction> {
        let Some(idx) = self.find_entry_index(name) else {
            self.error_message = Some(t!("characters.save.already_deleted").to_string());
            return None;
        };
        self.entries.remove(idx);
        dedupe_and_sort_entries(&mut self.entries);
        if let Err(err) = save_entries(project, &self.entries) {
            self.error_message = Some(tf!("characters.save.save_error", err = err));
            return None;
        }
        let image_path = image_path_for(project, name);
        let image_path_str = image_path.to_string_lossy();
        let _ = crate::storage::storage().remove_file(image_path_str.as_ref());
        self.clear_thumbnail(name);
        self.rebuild_group_filters();
        self.info_message = Some(t!("characters.save.deleted").to_string());
        self.editor = None;
        Some(CharactersTabAction::CharactersChanged)
    }

    fn ensure_loaded(&mut self, project: &ProjectData) {
        let chars_dir = project.paths.characters_dir.clone();
        let needs_reload = self
            .loaded_chars_dir
            .as_ref()
            .map(|existing| existing != &chars_dir)
            .unwrap_or(true);
        if !needs_reload {
            return;
        }
        self.loaded_chars_dir = Some(chars_dir);
        self.search_query.clear();
        self.selected_group_filter = group_all().to_string();
        self.error_message = None;
        self.info_message = None;
        self.editor = None;
        self.pending_overwrite = None;
        self.pending_delete_name = None;
        self.clear_all_thumbnails();
        self.clipboard_in_flight = false;
        self.clipboard_rx = None;

        match load_entries(project) {
            Ok(entries) => {
                self.entries = entries;
                self.rebuild_group_filters();
            }
            Err(err) => {
                self.entries.clear();
                self.rebuild_group_filters();
                self.error_message = Some(tf!("characters.save.load_error", err = err));
            }
        }
    }

    fn rebuild_group_filters(&mut self) {
        let mut groups: HashSet<String> = HashSet::new();
        for entry in &self.entries {
            for group in &entry.groups {
                let trimmed = group.trim();
                if !trimmed.is_empty() {
                    groups.insert(trimmed.to_string());
                }
            }
        }
        let mut values: Vec<String> = groups.into_iter().collect();
        values.sort_by_key(|v| v.to_lowercase());
        values.insert(0, group_all().to_string());

        if !values.contains(&self.selected_group_filter) {
            self.selected_group_filter = group_all().to_string();
        }
        self.groups_filter_values = values;
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let term = self.search_query.trim().to_lowercase();
        let selected_group = self.selected_group_filter.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let haystack = format!(
                    "{} {} {}",
                    entry.name,
                    entry.description,
                    entry.groups.join(" ")
                )
                .to_lowercase();
                let by_term = term.is_empty() || haystack.contains(&term);
                let by_group = selected_group == group_all()
                    || entry
                        .groups
                        .iter()
                        .any(|group| group.trim().to_lowercase() == selected_group);
                if by_term && by_group { Some(idx) } else { None }
            })
            .collect()
    }

    fn find_entry_index(&self, name: &str) -> Option<usize> {
        let key = name.to_lowercase();
        self.entries
            .iter()
            .position(|entry| entry.name.to_lowercase() == key)
    }

    fn ensure_thumbnail_requested(&mut self, name: &str, path: PathBuf) {
        if self.thumb_requested.contains(name) {
            return;
        }
        self.thumb_requested.insert(name.to_string());
        self.thumb_state
            .insert(name.to_string(), ThumbnailState::Loading);
        let _ = self.thumb_tx.send(ThumbnailJob::Load {
            name: name.to_string(),
            path,
        });
    }

    fn poll_thumbnail_results(&mut self, ctx: &egui::Context) {
        loop {
            match self.thumb_rx.try_recv() {
                Ok(result) => {
                    if result.missing {
                        self.thumb_state
                            .insert(result.name.clone(), ThumbnailState::Missing);
                        continue;
                    }
                    if result.error.is_some() {
                        self.thumb_state
                            .insert(result.name.clone(), ThumbnailState::Failed);
                        continue;
                    }
                    if let Some(decoded) = result.decoded {
                        let color = ColorImage::from_rgba_unmultiplied(
                            [decoded.width, decoded.height],
                            &decoded.rgba,
                        );
                        self.thumb_texture_serial = self.thumb_texture_serial.saturating_add(1);
                        let texture = ctx.load_texture(
                            format!(
                                "characters-thumb-{}-{}",
                                sanitize_texture_id(&result.name),
                                self.thumb_texture_serial
                            ),
                            color,
                            TextureOptions::LINEAR,
                        );
                        self.thumb_state
                            .insert(result.name.clone(), ThumbnailState::Ready(texture));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }

    fn start_clipboard_paste_request(&mut self) {
        if self.clipboard_in_flight {
            return;
        }
        let (tx, rx) = mpsc::channel::<ClipboardImageResult>();
        self.clipboard_rx = Some(rx);
        self.clipboard_in_flight = true;
        thread::spawn(move || {
            let response = match read_image_from_clipboard() {
                Ok(decoded) => ClipboardImageResult {
                    decoded: Some(decoded),
                    error: None,
                },
                Err(err) => ClipboardImageResult {
                    decoded: None,
                    error: Some(err),
                },
            };
            let _ = tx.send(response);
        });
    }

    fn poll_clipboard_result(&mut self) {
        let Some(rx) = self.clipboard_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(res) => Some(res),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ClipboardImageResult {
                decoded: None,
                error: Some(t!("characters.clipboard.thread_interrupted").to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.clipboard_in_flight = false;
        self.clipboard_rx = None;

        if let Some(decoded) = result.decoded {
            if let Some(editor) = self.editor.as_mut() {
                editor.pending_image = Some(ColorImage::from_rgba_unmultiplied(
                    [decoded.width, decoded.height],
                    &decoded.rgba,
                ));
                editor.pending_texture = None;
                editor.pending_image_rev = editor.pending_image_rev.saturating_add(1);
                editor.remove_image = false;
            }
            self.error_message = None;
            self.info_message = Some(t!("characters.clipboard.image_pasted").to_string());
        } else if let Some(err) = result.error {
            self.error_message = Some(tf!("characters.clipboard.paste_error", err = err));
        }
    }

    fn clear_thumbnail(&mut self, name: &str) {
        self.thumb_state.remove(name);
        self.thumb_requested.remove(name);
    }

    fn clear_all_thumbnails(&mut self) {
        self.thumb_state.clear();
        self.thumb_requested.clear();
    }
}

fn draw_editor_preview(ui: &mut egui::Ui, ctx: &egui::Context, editor: &mut CharacterEditorState) {
    let size = egui::vec2(EDITOR_PREVIEW_SIDE_PX, EDITOR_PREVIEW_SIDE_PX);
    ui.group(|ui| {
        ui.set_min_size(size + egui::vec2(16.0, 16.0));
        if let Some(image) = editor.pending_image.as_ref() {
            if editor.texture_rev != editor.pending_image_rev || editor.pending_texture.is_none() {
                editor.texture_rev = editor.pending_image_rev;
                editor.pending_texture = Some(ctx.load_texture(
                    format!("characters-editor-preview-{}", editor.pending_image_rev),
                    image.clone(),
                    TextureOptions::LINEAR,
                ));
            }
            if let Some(tex) = editor.pending_texture.as_ref() {
                ui.add(egui::Image::new((tex.id(), size)));
                ui.label(t!("characters.editor.preview_will_save"));
                return;
            }
        }
        if editor.remove_image {
            ui.label(t!("characters.editor.preview_will_delete"));
        } else {
            ui.label(t!("characters.editor.preview_none_selected"));
        }
    });
}

fn spawn_thumbnail_worker() -> (
    Sender<ThumbnailJob>,
    Receiver<ThumbnailResult>,
    JoinHandle<()>,
) {
    let (tx_job, rx_job) = mpsc::channel::<ThumbnailJob>();
    let (tx_result, rx_result) = mpsc::channel::<ThumbnailResult>();
    let handle = thread::spawn(move || {
        while let Ok(job) = rx_job.recv() {
            match job {
                ThumbnailJob::Stop => break,
                ThumbnailJob::Load { name, path } => {
                    let path_str = path.to_string_lossy();
                    if !crate::storage::storage().exists(path_str.as_ref()) {
                        let _ = tx_result.send(ThumbnailResult {
                            name,
                            decoded: None,
                            missing: true,
                            error: None,
                        });
                        continue;
                    }
                    match load_thumb_image(&path) {
                        Ok(decoded) => {
                            let _ = tx_result.send(ThumbnailResult {
                                name,
                                decoded: Some(decoded),
                                missing: false,
                                error: None,
                            });
                        }
                        Err(err) => {
                            let _ = tx_result.send(ThumbnailResult {
                                name,
                                decoded: None,
                                missing: false,
                                error: Some(err),
                            });
                        }
                    }
                }
            }
        }
    });
    (tx_job, rx_result, handle)
}

fn load_thumb_image(path: &Path) -> Result<DecodedImage, String> {
    let img = image::open(path).map_err(|err| err.to_string())?;
    let thumb = img
        .thumbnail(CARD_THUMB_SIDE_PX, CARD_THUMB_SIDE_PX)
        .to_rgba8();
    let width = thumb.width() as usize;
    let height = thumb.height() as usize;
    Ok(DecodedImage {
        width,
        height,
        rgba: thumb.into_raw(),
    })
}

fn load_image_from_path(path: &Path) -> Result<ColorImage, String> {
    let img = image::open(path).map_err(|err| err.to_string())?;
    let rgba = img.to_rgba8();
    let width = rgba.width() as usize;
    let height = rgba.height() as usize;
    Ok(ColorImage::from_rgba_unmultiplied(
        [width, height],
        &rgba.into_raw(),
    ))
}

fn read_image_from_clipboard() -> Result<DecodedImage, String> {
    let image = paste_image::read_image_from_clipboard()?;
    Ok(DecodedImage {
        width: image.width,
        height: image.height,
        rgba: image.rgba,
    })
}

fn save_color_image_png(path: &Path, image: &ColorImage) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let parent_str = parent.to_string_lossy();
        crate::storage::storage()
            .create_dir_all(parent_str.as_ref())
            .map_err(|err| err.to_string())?;
    }
    let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
    for px in &image.pixels {
        rgba.extend_from_slice(&px.to_array());
    }
    image::save_buffer_with_format(
        path,
        &rgba,
        image.size[0] as u32,
        image.size[1] as u32,
        image::ColorType::Rgba8,
        ImageFormat::Png,
    )
    .map_err(|err| err.to_string())
}

fn load_entries(project: &ProjectData) -> Result<Vec<CharacterEntry>, String> {
    let store = crate::storage::storage();
    let chars_dir = &project.paths.characters_dir;
    let chars_dir_str = chars_dir.to_string_lossy();
    store
        .create_dir_all(chars_dir_str.as_ref())
        .map_err(|err| err.to_string())?;
    let json_path = json_path_for(project);
    let json_path_str = json_path.to_string_lossy();
    if store.exists(json_path_str.as_ref()) {
        let raw = store
            .read_to_string(json_path_str.as_ref())
            .map_err(|err| err.to_string())?;
        if let Ok(parsed) = serde_json::from_str::<Vec<CharacterEntry>>(&raw) {
            let mut normalized = parsed
                .into_iter()
                .filter_map(|entry| {
                    let name = entry.name.trim().to_string();
                    if name.is_empty() {
                        return None;
                    }
                    Some(CharacterEntry {
                        name,
                        description: entry.description,
                        groups: normalize_groups(entry.groups),
                    })
                })
                .collect::<Vec<_>>();
            dedupe_and_sort_entries(&mut normalized);
            return Ok(normalized);
        }
    }

    let mut from_txt = Vec::new();
    let mut txt_files_to_remove: Vec<PathBuf> = Vec::new();
    if let Ok(dir_entries) = store.read_dir(chars_dir_str.as_ref()) {
        for dir_entry in dir_entries {
            // Storage lists only files and directories; treat every non-directory
            // as a file, mirroring the previous `!path.is_file()` skip.
            if dir_entry.is_dir {
                continue;
            }
            // Rebuild the full path so extension/stem parsing and reads stay identical.
            let path = chars_dir.join(&dir_entry.name);
            let is_txt = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase() == "txt")
                .unwrap_or(false);
            if !is_txt {
                continue;
            }
            let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            let path_str = path.to_string_lossy();
            let description = store
                .read_to_string(path_str.as_ref())
                .unwrap_or_default()
                .trim()
                .to_string();
            from_txt.push(CharacterEntry {
                name: stem,
                description,
                groups: Vec::new(),
            });
            txt_files_to_remove.push(path);
        }
    }
    dedupe_and_sort_entries(&mut from_txt);
    save_entries(project, &from_txt)?;
    for path in txt_files_to_remove {
        let path_str = path.to_string_lossy();
        let _ = store.remove_file(path_str.as_ref());
    }
    Ok(from_txt)
}

fn save_entries(project: &ProjectData, entries: &[CharacterEntry]) -> Result<(), String> {
    let store = crate::storage::storage();
    let chars_dir_str = project.paths.characters_dir.to_string_lossy();
    store
        .create_dir_all(chars_dir_str.as_ref())
        .map_err(|err| err.to_string())?;
    let path = json_path_for(project);
    let tmp = path.with_extension("json.tmp");
    let path_str = path.to_string_lossy();
    let tmp_str = tmp.to_string_lossy();
    let raw = serde_json::to_string_pretty(entries).map_err(|err| err.to_string())?;
    store
        .write(tmp_str.as_ref(), raw.as_bytes())
        .map_err(|err| err.to_string())?;
    if store.exists(path_str.as_ref()) {
        store
            .remove_file(path_str.as_ref())
            .map_err(|err| err.to_string())?;
    }
    store
        .rename(tmp_str.as_ref(), path_str.as_ref())
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn dedupe_and_sort_entries(entries: &mut Vec<CharacterEntry>) {
    let mut map = HashMap::new();
    for entry in entries.drain(..) {
        map.insert(entry.name.to_lowercase(), entry);
    }
    let mut values: Vec<CharacterEntry> = map.into_values().collect();
    values.sort_by_key(|entry| entry.name.to_lowercase());
    *entries = values;
}

fn normalize_groups(values: Vec<String>) -> Vec<String> {
    let mut used = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_lowercase();
        if used.insert(key) {
            out.push(trimmed.to_string());
        }
    }
    out.sort_by_key(|v| v.to_lowercase());
    out
}

fn safe_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        if matches!(
            ch,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\n' | '\r' | '\t'
        ) {
            continue;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
        if out.chars().count() >= MAX_NAME_LEN {
            break;
        }
    }
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

fn sanitize_texture_id(name: &str) -> String {
    name.chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { '_' })
        .collect()
}

fn json_path_for(project: &ProjectData) -> PathBuf {
    project.paths.characters_dir.join("characters.json")
}

fn image_path_for(project: &ProjectData, name: &str) -> PathBuf {
    project.paths.characters_dir.join(format!("{name}.png"))
}
