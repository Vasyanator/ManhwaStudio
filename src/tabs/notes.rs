/*
FILE OVERVIEW: src/tabs/notes.rs
Translation notes tab with two sub-tabs:
- "Собранный промпт": builds final prompt from `notes_file` template + `{charas}/{terms}`.
- "Шаблон (notes_file)": edits the template file itself with placeholder helpers.

Data sources:
- Template: `project.paths.notes_file`.
- Characters: loaded through `tabs::characters::load_characters_for_notes`.
- Terms: loaded through `tabs::terms::load_terms_for_notes`.

Performance:
- Prompt assembly and file reads are executed in a background worker thread.
- GUI thread only renders state and polls worker results.
*/

use crate::project::ProjectData;
use crate::tabs::characters::{CharacterNoteEntry, load_characters_for_notes};
use crate::tabs::terms::{TermNoteEntry, load_terms_for_notes};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use ms_thread as thread;
use web_time::{Duration, Instant, SystemTime};

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(600);
const COPY_FEEDBACK_DURATION: Duration = Duration::from_millis(800);
const SAVE_FEEDBACK_DURATION: Duration = Duration::from_millis(900);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum NotesSubTab {
    #[default]
    Prompt,
    Template,
}

#[derive(Debug, Clone, Eq, PartialEq, Default)]
struct FileSignature {
    exists: bool,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug)]
struct ComposeResult {
    preview_text: String,
    template_text: String,
    warnings: Vec<String>,
    template_sig: FileSignature,
    chars_sig: FileSignature,
    terms_sig: FileSignature,
}

#[derive(Debug)]
pub struct NotesTabState {
    linked_character: Option<String>,
    characters_revision: u64,
    terms_revision: u64,
    active_subtab: NotesSubTab,
    include_characters: bool,
    include_terms: bool,
    preview_text: String,
    template_editor_text: String,
    editor_dirty: bool,
    compose_rx: Option<Receiver<ComposeResult>>,
    compose_in_flight: bool,
    refresh_requested: bool,
    info_message: Option<String>,
    error_message: Option<String>,
    copied_at: Option<Instant>,
    saved_at: Option<Instant>,
    loaded_notes_path: Option<PathBuf>,
    template_sig: FileSignature,
    chars_sig: FileSignature,
    terms_sig: FileSignature,
    last_watch_poll: Option<Instant>,
}

impl Default for NotesTabState {
    fn default() -> Self {
        Self {
            linked_character: None,
            characters_revision: 0,
            terms_revision: 0,
            active_subtab: NotesSubTab::Prompt,
            include_characters: true,
            include_terms: true,
            preview_text: String::new(),
            template_editor_text: String::new(),
            editor_dirty: false,
            compose_rx: None,
            compose_in_flight: false,
            refresh_requested: true,
            info_message: None,
            error_message: None,
            copied_at: None,
            saved_at: None,
            loaded_notes_path: None,
            template_sig: FileSignature::default(),
            chars_sig: FileSignature::default(),
            terms_sig: FileSignature::default(),
            last_watch_poll: None,
        }
    }
}

impl NotesTabState {
    pub fn set_character_context(&mut self, name: String) {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            self.linked_character = None;
        } else {
            self.linked_character = Some(trimmed.to_string());
        }
    }

    pub fn notify_characters_changed(&mut self) {
        self.characters_revision = self.characters_revision.saturating_add(1);
        self.refresh_requested = true;
    }

    pub fn notify_terms_changed(&mut self) {
        self.terms_revision = self.terms_revision.saturating_add(1);
        self.refresh_requested = true;
    }

    pub fn draw(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, project: &ProjectData) {
        self.ensure_project_loaded(project);
        self.poll_compose_result(ctx);
        self.poll_external_changes(project);
        self.ensure_compose_started(project);

        let copy_label = if self
            .copied_at
            .map(|t| t.elapsed() <= COPY_FEEDBACK_DURATION)
            .unwrap_or(false)
        {
            t!("notes.prompt.copied")
        } else {
            t!("notes.prompt.copy_button")
        };
        let save_label = if self
            .saved_at
            .map(|t| t.elapsed() <= SAVE_FEEDBACK_DURATION)
            .unwrap_or(false)
        {
            t!("notes.template.saved_flash")
        } else {
            t!("notes.template.save_button")
        };

        ui.vertical(|ui| {
            ui.heading(t!("notes.heading"));
            if let Some(name) = &self.linked_character {
                ui.label(
                    egui::RichText::new(tf!("notes.character_context", name = name)).strong(),
                );
            }
            ui.small(tf!("notes.revisions", characters = self.characters_revision, terms = self.terms_revision));
            ui.small(tf!("notes.paths_tooltip", template = project.paths.notes_file.display(), characters = project.paths.characters_dir.join("characters.json").display(), terms = project.paths.terms_file.display()));

            if let Some(msg) = &self.info_message {
                ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
            }
            if let Some(err) = &self.error_message {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), err);
            }
            if self.compose_in_flight {
                ui.small(t!("notes.prompt.updating"));
            }

            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.active_subtab,
                    NotesSubTab::Prompt,
                    t!("notes.tab.assembled_prompt"),
                );
                ui.selectable_value(
                    &mut self.active_subtab,
                    NotesSubTab::Template,
                    t!("notes.tab.template"),
                );
            });
            ui.separator();

            match self.active_subtab {
                NotesSubTab::Prompt => {
                    ui.horizontal_wrapped(|ui| {
                        let toggled_char = ui
                            .checkbox(&mut self.include_characters, t!("notes.insert_characters_label"))
                            .changed();
                        let toggled_terms = ui
                            .checkbox(&mut self.include_terms, t!("notes.insert_terms_label"))
                            .changed();
                        if toggled_char || toggled_terms {
                            self.refresh_requested = true;
                            self.error_message = None;
                        }
                        if ui.button(t!("notes.prompt.refresh_button")).clicked() {
                            self.refresh_requested = true;
                            self.error_message = None;
                        }
                        if ui.button(copy_label).clicked() {
                            ctx.copy_text(self.preview_text.clone());
                            self.copied_at = Some(Instant::now());
                        }
                    });

                    ui.small(
                        t!("notes.template.placeholder_hint"),
                    );
                    ui.add_space(6.0);
                    ui.add(
                        egui::TextEdit::multiline(&mut self.preview_text)
                            .desired_rows(20)
                            .interactive(false)
                            .desired_width(f32::INFINITY),
                    );
                }
                NotesSubTab::Template => {
                    let has_charas = self.template_editor_text.contains("{charas}");
                    let has_terms = self.template_editor_text.contains("{terms}");
                    if !has_charas && !has_terms {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    egui::RichText::new(
                                        t!("notes.template.no_placeholders_hint"),
                                    )
                                    .color(egui::Color32::from_rgb(210, 150, 40)),
                                );
                                if ui.button(t!("notes.template.insert_charas_button")).clicked() {
                                    insert_placeholder(&mut self.template_editor_text, "{charas}");
                                    self.editor_dirty = true;
                                }
                                if ui.button(t!("notes.template.insert_terms_button")).clicked() {
                                    insert_placeholder(&mut self.template_editor_text, "{terms}");
                                    self.editor_dirty = true;
                                }
                            });
                        });
                        ui.add_space(8.0);
                    }

                    let editor_resp = ui.add(
                        egui::TextEdit::multiline(&mut self.template_editor_text)
                            .desired_rows(22)
                            .desired_width(f32::INFINITY),
                    );
                    if editor_resp.changed() {
                        self.editor_dirty = true;
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(save_label).clicked()
                            && self.save_template(project).is_ok() {
                                self.saved_at = Some(Instant::now());
                            }
                        if self.editor_dirty {
                            ui.small(t!("notes.template.unsaved_changes"));
                        }
                    });
                }
            }
        });

        if self
            .copied_at
            .map(|t| t.elapsed() <= COPY_FEEDBACK_DURATION)
            .unwrap_or(false)
            || self
                .saved_at
                .map(|t| t.elapsed() <= SAVE_FEEDBACK_DURATION)
                .unwrap_or(false)
            || self.compose_in_flight
        {
            ctx.request_repaint();
        }
    }

    fn ensure_project_loaded(&mut self, project: &ProjectData) {
        let path = project.paths.notes_file.clone();
        let changed = self
            .loaded_notes_path
            .as_ref()
            .map(|loaded| loaded != &path)
            .unwrap_or(true);
        if !changed {
            return;
        }
        self.loaded_notes_path = Some(path);
        self.active_subtab = NotesSubTab::Prompt;
        self.include_characters = true;
        self.include_terms = true;
        self.preview_text.clear();
        self.template_editor_text.clear();
        self.editor_dirty = false;
        self.compose_rx = None;
        self.compose_in_flight = false;
        self.refresh_requested = true;
        self.info_message = None;
        self.error_message = None;
        self.copied_at = None;
        self.saved_at = None;
        self.template_sig = read_file_signature(&project.paths.notes_file);
        self.chars_sig = read_file_signature(&project.paths.characters_dir.join("characters.json"));
        self.terms_sig = read_file_signature(&project.paths.terms_file);
        self.last_watch_poll = None;
    }

    fn ensure_compose_started(&mut self, project: &ProjectData) {
        if self.compose_in_flight || !self.refresh_requested {
            return;
        }
        self.refresh_requested = false;
        let project_snapshot = project.clone();
        let include_characters = self.include_characters;
        let include_terms = self.include_terms;
        let (tx, rx) = mpsc::channel::<ComposeResult>();
        self.compose_rx = Some(rx);
        self.compose_in_flight = true;
        thread::spawn(move || {
            let template_path = project_snapshot.paths.notes_file.clone();
            let chars_path = project_snapshot
                .paths
                .characters_dir
                .join("characters.json");
            let terms_path = project_snapshot.paths.terms_file.clone();
            let template_text = read_text_fallback(&template_path).unwrap_or_default();
            let mut warnings = Vec::new();

            let chars = match load_characters_for_notes(&project_snapshot) {
                Ok(v) => v,
                Err(err) => {
                    warnings.push(tf!("notes.compose.characters_error", err = err));
                    Vec::new()
                }
            };
            let terms = match load_terms_for_notes(&project_snapshot) {
                Ok(v) => v,
                Err(err) => {
                    warnings.push(tf!("notes.compose.terms_error", err = err));
                    Vec::new()
                }
            };

            let preview_text = compose_preview(
                &template_text,
                &chars,
                &terms,
                include_characters,
                include_terms,
            );
            let _ = tx.send(ComposeResult {
                preview_text,
                template_text,
                warnings,
                template_sig: read_file_signature(&template_path),
                chars_sig: read_file_signature(&chars_path),
                terms_sig: read_file_signature(&terms_path),
            });
        });
    }

    fn poll_compose_result(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.compose_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(res) => Some(res),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ComposeResult {
                preview_text: String::new(),
                template_text: String::new(),
                warnings: vec![t!("notes.compose.thread_interrupted").to_string()],
                template_sig: FileSignature::default(),
                chars_sig: FileSignature::default(),
                terms_sig: FileSignature::default(),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.compose_rx = None;
        self.compose_in_flight = false;
        self.preview_text = result.preview_text;
        if !self.editor_dirty {
            self.template_editor_text = result.template_text;
        }
        self.template_sig = result.template_sig;
        self.chars_sig = result.chars_sig;
        self.terms_sig = result.terms_sig;
        if result.warnings.is_empty() {
            self.error_message = None;
        } else {
            self.error_message = Some(tf!("notes.compose.warnings", warnings = result.warnings.join("; ")));
        }
        ctx.request_repaint();
    }

    fn poll_external_changes(&mut self, project: &ProjectData) {
        let now = Instant::now();
        if self
            .last_watch_poll
            .map(|t| now.duration_since(t) < WATCH_POLL_INTERVAL)
            .unwrap_or(false)
        {
            return;
        }
        self.last_watch_poll = Some(now);

        let template_sig_now = read_file_signature(&project.paths.notes_file);
        let chars_sig_now =
            read_file_signature(&project.paths.characters_dir.join("characters.json"));
        let terms_sig_now = read_file_signature(&project.paths.terms_file);

        let template_changed = template_sig_now != self.template_sig;
        let chars_changed = chars_sig_now != self.chars_sig;
        let terms_changed = terms_sig_now != self.terms_sig;
        if template_changed || chars_changed || terms_changed {
            self.refresh_requested = true;
        }
    }

    fn save_template(&mut self, project: &ProjectData) -> Result<(), String> {
        let store = crate::storage::storage();
        let path = &project.paths.notes_file;
        if let Some(parent) = path.parent() {
            let parent_str = parent.to_string_lossy();
            store
                .create_dir_all(parent_str.as_ref())
                .map_err(|err| err.to_string())?;
        }
        let path_str = path.to_string_lossy();
        store
            .write(path_str.as_ref(), self.template_editor_text.as_bytes())
            .map_err(|err| err.to_string())?;
        self.editor_dirty = false;
        self.info_message = Some(t!("notes.template.saved").to_string());
        self.error_message = None;
        self.refresh_requested = true;
        Ok(())
    }
}

fn compose_preview(
    template: &str,
    characters: &[CharacterNoteEntry],
    terms: &[TermNoteEntry],
    include_characters: bool,
    include_terms: bool,
) -> String {
    let chars_block = if include_characters {
        build_characters_block(characters)
    } else {
        String::new()
    };
    let terms_block = if include_terms {
        build_terms_block(terms)
    } else {
        String::new()
    };

    let mut text = template.to_string();
    let (with_chars, used_chars) = safe_replace(&text, "{charas}", &chars_block);
    text = with_chars;
    let (with_terms, used_terms) = safe_replace(&text, "{terms}", &terms_block);
    text = with_terms;

    let mut tail_parts = Vec::new();
    if include_characters && !used_chars && !chars_block.trim().is_empty() {
        tail_parts.push(chars_block.trim().to_string());
    }
    if include_terms && !used_terms && !terms_block.trim().is_empty() {
        tail_parts.push(terms_block.trim().to_string());
    }
    if !tail_parts.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text = format!("{}\n", text.trim_end());
        }
        text.push_str(&tail_parts.join("\n"));
        text.push('\n');
    }
    text
}

fn build_characters_block(entries: &[CharacterNoteEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut items = entries.to_vec();
    items.sort_by_key(|entry| entry.name.to_lowercase());

    let mut lines = vec![t!("notes.block.characters_heading").to_string(), String::new()];
    for item in items {
        if item.name.trim().is_empty() {
            continue;
        }
        if item.groups.is_empty() {
            lines.push(format!("**{}**", item.name.trim()));
        } else {
            lines.push(tf!("notes.block.character_line", name = item.name.trim(), groups = item.groups.join(", ")));
        }
        let desc = item.description.trim();
        if desc.is_empty() {
            lines.push(t!("notes.block.no_description").to_string());
        } else {
            lines.push(desc.to_string());
        }
        lines.push(String::new());
    }
    if lines.len() <= 2 {
        return String::new();
    }
    let mut out = lines.join("\n");
    out = out.trim_end().to_string();
    out.push('\n');
    out
}

fn build_terms_block(entries: &[TermNoteEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut items = entries.to_vec();
    items.sort_by_key(|entry| entry.name.to_lowercase());

    let mut lines = vec![t!("notes.block.terms_heading").to_string(), String::new()];
    for item in items {
        let name = item.name.trim();
        if name.is_empty() {
            continue;
        }
        lines.push(format!("**{name}**"));
        let orig = item.orig_name.trim();
        if !orig.is_empty() {
            lines.push(tf!("notes.block.term_orig_name", orig = orig));
        }
        let desc = item.description.trim();
        if !desc.is_empty() {
            lines.push(tf!("notes.block.term_description", desc = desc));
        }
        if !item.tags.is_empty() {
            lines.push(tf!("notes.block.term_tags", tags = item.tags.join(", ")));
        }
        lines.push(String::new());
    }
    if lines.len() <= 2 {
        return String::new();
    }
    let mut out = lines.join("\n");
    out = out.trim_end().to_string();
    out.push('\n');
    out
}

fn safe_replace(text: &str, placeholder: &str, replacement: &str) -> (String, bool) {
    if text.contains(placeholder) {
        (text.replace(placeholder, replacement), true)
    } else {
        (text.to_string(), false)
    }
}

fn insert_placeholder(target: &mut String, placeholder: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(placeholder);
}

fn read_text_fallback(path: &Path) -> Option<String> {
    let path_str = path.to_string_lossy();
    let bytes = crate::storage::storage().read(path_str.as_ref()).ok()?;
    if let Ok(utf8) = String::from_utf8(bytes.clone()) {
        return Some(utf8);
    }
    if let Ok(cp1251) = decode_cp1251(&bytes) {
        return Some(cp1251);
    }
    Some(String::from_utf8_lossy(&bytes).to_string())
}

fn decode_cp1251(bytes: &[u8]) -> Result<String, String> {
    const CP1251_TABLE: [char; 128] = [
        '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}',
        '\u{040B}', '\u{040F}', '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{0000}', '\u{2122}', '\u{0459}', '\u{203A}',
        '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}', '\u{00A0}', '\u{040E}', '\u{045E}',
        '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}', '\u{0401}', '\u{00A9}',
        '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}', '\u{00B0}',
        '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
        '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}',
        '\u{0457}', '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}',
        '\u{0416}', '\u{0417}', '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}',
        '\u{041D}', '\u{041E}', '\u{041F}', '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}',
        '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}', '\u{0428}', '\u{0429}', '\u{042A}',
        '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}', '\u{0430}', '\u{0431}',
        '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}', '\u{0438}',
        '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
        '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}',
        '\u{0447}', '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}',
        '\u{044E}', '\u{044F}',
    ];

    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x80 {
            out.push(b as char);
        } else {
            let mapped = CP1251_TABLE[(b - 0x80) as usize];
            if mapped == '\u{0000}' {
                return Err("encountered undefined CP1251 code point".to_string());
            }
            out.push(mapped);
        }
    }
    Ok(out)
}

fn read_file_signature(path: &Path) -> FileSignature {
    let path_str = path.to_string_lossy();
    let Ok(meta) = crate::storage::storage().metadata(path_str.as_ref()) else {
        return FileSignature::default();
    };
    // `Metadata::modified` is `Some` on the native/desktop backend and `None`
    // on the web in-memory backend (no filesystem mtime). The watcher keys off
    // whatever the backend provides plus length, degrading gracefully on web.
    FileSignature {
        exists: true,
        len: meta.len,
        modified: meta.modified,
    }
}
