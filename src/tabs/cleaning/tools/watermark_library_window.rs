/*
FILE HEADER (cleaning/tools/watermark_library_window.rs)

Purpose:
The management window of the watermark library — the screen that turns the on-disk entries
of `watermark_library.rs` into something a user can read and curate. Opened from the
«Удаление водяных знаков» tool (`watermark_removal.rs`); it is a tool-owned `egui::Window`,
NOT a panel-dock tab, because nothing on it needs docking, tearing off or persisting a
layout (`dev-docs/watermark_library_plan.md`, "UI"; precedent:
`tabs/settings/typesetting/font_properties_window.rs`).

Main responsibilities:
- list every entry with its preview, its VERBATIM display name, its quality verdict, the
  background levels it was calibrated on, and where it has been used;
- offer rename, delete, export (archive or folder), import, creation from reference crops,
  and improvement of an existing entry with one more level;
- run every one of those off the GUI thread and poll the channel per frame.

Key structures:
- `WatermarkLibraryWindow` — the whole state, owned by `WatermarkRemovalTool`.
- `IntakeForm` — the reference-crop form (picked files plus the new entry's name).
- `LibraryEvent` — the worker protocol; `EntryAction` — what one row asked for.

Key functions:
- `show()` (draws the window and polls its jobs), `take_changed()` (tells the tool the
  on-disk library moved, so its own picker reloads).

Notes:
- The QUALITY column is the point of the screen. It reports the engine's own graded verdict
  rebuilt from the stored metadata (`watermark_entry::conditioning_from_stored`), so the
  wording cannot drift from the chapter mode's: the IMPRINT is what is measured exactly,
  never «c»; the stated ±% bounds the alpha SCALE only. For a graded entry it also names the
  background that would make it exact, which `ModelConditioning::suggested_background()`
  provides.
- A display name is USER DATA. The editor keeps a pending copy per entry id and writes it
  back VERBATIM — no trim, no case folding, no normalization.
- Deletion is confirmed inline (a second click on an armed button) rather than through a
  native modal, which would block the GUI thread.
*/
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use eframe::egui;
use egui::{Color32, TextureHandle, TextureOptions};
use ms_thread as thread;

use super::watermark_entry::{
    ReferenceIntakeRequest, conditioning_from_stored, run_reference_intake,
};
use super::watermark_library::{
    ENTRY_ARCHIVE_EXTENSION, EntrySummary, delete_entry, export_entry_dir, export_entry_zip,
    import_entry, list_entries, load_entry, load_entry_template, rename_entry, save_entry,
};
use crate::tabs::cleaning::watermark_chapter::{ModelConditioning, SampleParams};

/// Side of one entry's preview thumbnail, points.
const PREVIEW_SIDE: f32 = 72.0;
/// Largest side the decoded template is downscaled to before it becomes a texture, pixels.
/// A template is small already; this only stops a 512-px one from costing four textures'
/// worth of VRAM for a 72-point thumbnail.
const PREVIEW_MAX_PX: u32 = 128;
/// Width of the name editor, points.
const NAME_EDIT_WIDTH: f32 = 240.0;
/// Colour of the "exact" verdict line. Matches the affirmative green used elsewhere in the
/// cleaning UI rather than introducing a new one.
const VERDICT_EXACT_RGB: [u8; 3] = [120, 200, 120];
/// Colour of a graded verdict line — the same amber the experimental-mode warning uses.
const VERDICT_GRADED_RGB: [u8; 3] = [255, 170, 60];

/// What a file picker was opened for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerPurpose {
    /// Image files for a new entry built from reference crops.
    ReferenceCrops,
    /// Image files that add one more level to an existing entry.
    ImproveEntry(String),
    /// A `.zip` archive to import.
    ImportArchive,
    /// A directory holding an unpacked entry.
    ImportFolder,
    /// Where to write one entry's archive.
    ExportArchive(String),
    /// Where to copy one entry as a folder.
    ExportFolder(String),
}

/// Messages a library worker sends back.
enum LibraryEvent {
    List(Vec<EntrySummary>),
    Previews(Vec<(String, egui::ColorImage)>),
    /// A mutation finished; the message is already localized and the listing is now stale.
    Changed(String),
    Failed(String),
}

/// The reference-crop form.
#[derive(Debug, Default)]
struct IntakeForm {
    /// Shown only while the user is filling it in.
    open: bool,
    /// Display name of the entry the crops will create. Stored VERBATIM.
    name: String,
    /// Files picked so far, in pick order. The first one defines the footprint.
    files: Vec<PathBuf>,
    /// `Some(entry_id)` adds these crops to that entry instead of creating a new one.
    target: Option<String>,
}

impl IntakeForm {
    /// Clears the form back to "nothing picked".
    fn reset(&mut self) {
        self.open = false;
        self.name.clear();
        self.files.clear();
        self.target = None;
    }
}

/// The library management window.
#[derive(Default)]
pub(super) struct WatermarkLibraryWindow {
    open: bool,
    entries: Vec<EntrySummary>,
    /// Cleared until the first listing answered, so the window fills itself once.
    listed: bool,
    /// Pending name edits keyed by entry id. Only entries the user typed into appear here,
    /// so a listing refresh never fights the text cursor.
    edits: HashMap<String, String>,
    previews: HashMap<String, TextureHandle>,
    /// Entries a preview job has already been run for. Without it an entry whose template
    /// cannot be decoded would be retried — and a thread spawned — on every frame.
    previews_tried: HashSet<String>,
    confirm_delete: Option<String>,
    intake: IntakeForm,
    rx: Option<Receiver<LibraryEvent>>,
    status: Option<String>,
    picker_rx: Option<Receiver<Option<Vec<PathBuf>>>>,
    picker: Option<PickerPurpose>,
    /// Set whenever the on-disk library changed, so the chapter mode's own picker reloads.
    changed: bool,
    /// Screen rect of the window as it was drawn last frame. The cleaning canvas gates its
    /// own pointer handling on the tool's floating surfaces, so this window has to report
    /// where it is or a click inside it would also paint on the page underneath.
    rect: Option<egui::Rect>,
}

impl WatermarkLibraryWindow {
    /// Opens the window and marks its listing stale, so it always shows the current disk.
    pub(super) fn open(&mut self) {
        self.open = true;
        self.listed = false;
    }

    /// True while the window is on screen.
    #[must_use]
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// True when `pointer` is inside the window as it was drawn last frame.
    ///
    /// Used by the tool's canvas-capture answer: a click on this window must not also reach
    /// the page under it.
    #[must_use]
    pub(super) fn contains_pointer(&self, pointer: egui::Pos2) -> bool {
        self.open
            .then_some(self.rect)
            .flatten()
            .is_some_and(|rect| rect.contains(pointer))
    }

    /// Takes the "the library changed on disk" flag, clearing it.
    ///
    /// The chapter mode keeps its own copy of the entry list; this is how it learns that
    /// copy is stale without the two states having to be shared.
    pub(super) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// True while a worker or a file dialog owns the library state.
    fn busy(&self) -> bool {
        self.rx.is_some() || self.picker_rx.is_some()
    }

    /// Starts a worker, refusing a second job while one is in flight: two jobs would leave
    /// the list drawn from two different states of the disk.
    fn start_job<F>(&mut self, work: F)
    where
        F: FnOnce(&Sender<LibraryEvent>) + Send + 'static,
    {
        if self.rx.is_some() {
            self.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return;
        }
        let (tx, rx) = mpsc::channel::<LibraryEvent>();
        self.rx = Some(rx);
        thread::spawn(move || work(&tx));
    }

    /// Reloads the entry list from disk.
    fn request_list(&mut self) {
        self.listed = true;
        self.start_job(|tx| {
            let _ = tx.send(LibraryEvent::List(list_entries()));
        });
    }

    /// Decodes the templates of every listed entry that has no texture yet.
    ///
    /// One job for the whole batch: a template is a small PNG, and a job per row would spawn
    /// a thread per entry on the first frame the window is open.
    fn request_previews(&mut self) {
        let wanted: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| !self.previews_tried.contains(&entry.id))
            .map(|entry| entry.id.clone())
            .collect();
        if wanted.is_empty() {
            return;
        }
        self.previews_tried.extend(wanted.iter().cloned());
        self.start_job(move |tx| {
            let mut out = Vec::with_capacity(wanted.len());
            for id in wanted {
                match load_entry_template(&id) {
                    Ok(template) => out.push((id, thumbnail(&template))),
                    Err(err) => crate::runtime_log::log_warn(format!(
                        "[cleaning] watermark library preview for {id} failed: {err}"
                    )),
                }
            }
            let _ = tx.send(LibraryEvent::Previews(out));
        });
    }

    /// Drains the worker channel and folds every finished step into the state.
    fn poll_job(&mut self, ctx: &egui::Context) {
        loop {
            let event = {
                let Some(rx) = self.rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(LibraryEvent::List(entries)) => {
                    // Drop the textures and pending edits of entries that no longer exist,
                    // or the maps would grow one dead item per delete.
                    self.previews
                        .retain(|id, _| entries.iter().any(|entry| &entry.id == id));
                    self.previews_tried
                        .retain(|id| entries.iter().any(|entry| &entry.id == id));
                    self.edits
                        .retain(|id, _| entries.iter().any(|entry| &entry.id == id));
                    self.entries = entries;
                    self.finish_job();
                }
                Ok(LibraryEvent::Previews(previews)) => {
                    for (id, image) in previews {
                        let texture = ctx.load_texture(
                            format!("cleaning-watermark-library-{id}"),
                            image,
                            TextureOptions::NEAREST,
                        );
                        self.previews.insert(id, texture);
                    }
                    self.finish_job();
                }
                Ok(LibraryEvent::Changed(status)) => {
                    self.status = Some(status);
                    self.changed = true;
                    self.listed = false;
                    self.finish_job();
                }
                Ok(LibraryEvent::Failed(err)) => {
                    self.status = Some(tf!("cleaning.mask_editor.processing_error", err = err));
                    self.finish_job();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.status =
                        Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                    self.finish_job();
                    return;
                }
            }
        }
    }

    /// Clears the in-flight marker once a job reported its last event.
    fn finish_job(&mut self) {
        self.rx = None;
    }

    /// Folds a finished file pick into the form or starts the job it was picked for.
    fn poll_picker(&mut self) {
        let Some(rx) = self.picker_rx.as_ref() else {
            return;
        };
        let picked = match rx.try_recv() {
            Ok(picked) => picked,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.picker_rx = None;
        let Some(purpose) = self.picker.take() else {
            return;
        };
        let Some(paths) = picked.filter(|paths| !paths.is_empty()) else {
            return;
        };
        match purpose {
            PickerPurpose::ReferenceCrops => {
                self.intake.open = true;
                self.intake.target = None;
                self.intake.files = paths;
            }
            PickerPurpose::ImproveEntry(entry_id) => {
                self.intake.open = true;
                self.intake.target = Some(entry_id);
                self.intake.files = paths;
            }
            PickerPurpose::ImportArchive | PickerPurpose::ImportFolder => {
                let source = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match import_entry(&source) {
                        Ok(entry_id) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_imported_status",
                            id = entry_id
                        )),
                        Err(err) => LibraryEvent::Failed(err),
                    });
                });
            }
            PickerPurpose::ExportArchive(entry_id) => {
                let dest = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match export_entry_zip(&entry_id, &dest) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_exported_status",
                            path = dest.display()
                        )),
                        Err(err) => LibraryEvent::Failed(err),
                    });
                });
            }
            PickerPurpose::ExportFolder(entry_id) => {
                let parent = paths[0].clone();
                self.start_job(move |tx| {
                    let _ = tx.send(match export_entry_dir(&entry_id, &parent) {
                        Ok(path) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_exported_status",
                            path = path.display()
                        )),
                        Err(err) => LibraryEvent::Failed(err),
                    });
                });
            }
        }
    }

    /// Opens a native file dialog on a worker thread for `purpose`.
    fn start_picker(&mut self, purpose: PickerPurpose) {
        if self.picker_rx.is_some() {
            return;
        }
        self.picker_rx = Some(spawn_picker(&purpose));
        self.picker = Some(purpose);
    }

    /// Runs the reference-crop intake and writes the resulting entry.
    fn start_intake(&mut self, sample_params: SampleParams, max_side: u32) {
        let files = self.intake.files.clone();
        let name = self.intake.name.clone();
        let target = self.intake.target.clone();
        self.intake.reset();
        self.start_job(move |tx| {
            // Loading the base entry is I/O and belongs on this thread, not on the GUI one.
            let base = match target.as_deref().map(load_entry) {
                Some(Ok(entry)) => Some(entry),
                Some(Err(err)) => {
                    let _ = tx.send(LibraryEvent::Failed(err));
                    return;
                }
                None => None,
            };
            let outcome = run_reference_intake(ReferenceIntakeRequest {
                files,
                sample_params,
                base,
                name,
                max_side,
            });
            let event = match outcome.and_then(|outcome| {
                let levels = outcome
                    .conditioning
                    .levels()
                    .iter()
                    .map(|level| format!("{level:.0}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let crops = outcome.reports.len();
                save_entry(&outcome.request).map(|entry_id| {
                    tf!(
                        "cleaning.tools.watermark.chapter.reference_saved_status",
                        id = entry_id,
                        crops = crops,
                        levels = levels
                    )
                })
            }) {
                Ok(status) => LibraryEvent::Changed(status),
                Err(err) => LibraryEvent::Failed(err),
            };
            let _ = tx.send(event);
        });
    }

    /// Draws the window and drives its jobs. Call once per frame while the tool is active;
    /// it does nothing when the window is closed.
    ///
    /// `sample_params` and `max_side` come from the tool's own settings, so the intake and
    /// the chapter mode cannot disagree about the ring measurement or the footprint limit.
    pub(super) fn show(&mut self, ctx: &egui::Context, sample_params: SampleParams, max_side: u32) {
        if !self.open {
            return;
        }
        self.poll_job(ctx);
        self.poll_picker();
        if !self.listed && self.rx.is_none() {
            self.request_list();
        }
        if self.rx.is_none() {
            self.request_previews();
        }

        let mut open = true;
        let drawn = egui::Window::new(t!("cleaning.tools.watermark.chapter.library_window_title"))
            // The title is localized, so the id is pinned (`egui-docs/05-ids-and-i18n.md`).
            .id(egui::Id::new("cleaning.tools.watermark.library_window"))
            .open(&mut open)
            .collapsible(true)
            .resizable(true)
            .default_size([620.0, 560.0])
            // The entry list carries its own bounded scroll area; a second one on the window
            // would nest two vertical scrollers.
            .vscroll(false)
            .show(ctx, |ui| self.draw_body(ui, sample_params, max_side));
        self.rect = drawn.map(|inner| inner.response.rect);
        self.open = open;
        if !self.open {
            self.rect = None;
        }
        if self.busy() {
            ctx.request_repaint();
        }
    }

    /// Draws the whole window body: the action row, the status line, the reference-crop
    /// form and the entry list.
    fn draw_body(&mut self, ui: &mut egui::Ui, sample_params: SampleParams, max_side: u32) {
        let busy = self.busy();
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
                self.listed = false;
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.reference_add_button"
                    )),
                )
                .on_hover_text(t!("cleaning.tools.watermark.chapter.reference_add_hint"))
                .clicked()
            {
                self.start_picker(PickerPurpose::ReferenceCrops);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_import_zip_button"
                    )),
                )
                .clicked()
            {
                self.start_picker(PickerPurpose::ImportArchive);
            }
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!(
                        "cleaning.tools.watermark.chapter.library_import_dir_button"
                    )),
                )
                .clicked()
            {
                self.start_picker(PickerPurpose::ImportFolder);
            }
            if busy {
                ui.spinner();
            }
        });
        ui.small(t!("cleaning.tools.watermark.chapter.reference_help_hint"));
        if let Some(status) = self.status.as_ref() {
            ui.small(status);
        }
        self.draw_intake(ui, sample_params, max_side);
        ui.separator();

        if self.entries.is_empty() {
            ui.small(t!("cleaning.tools.watermark.chapter.library_empty_hint"));
            return;
        }
        let mut action: Option<EntryAction> = None;
        egui::ScrollArea::vertical()
            .id_salt("cleaning.tools.watermark.library_window_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for index in 0..self.entries.len() {
                    ui.push_id(index, |ui| {
                        if let Some(requested) = self.draw_entry(ui, index, busy) {
                            action = Some(requested);
                        }
                    });
                }
            });
        if let Some(action) = action {
            self.run_entry_action(action);
        }
    }

    /// Draws the reference-crop form: the picked files, the new entry's name and the
    /// button that starts the intake.
    fn draw_intake(&mut self, ui: &mut egui::Ui, sample_params: SampleParams, max_side: u32) {
        if !self.intake.open {
            return;
        }
        // Everything the form draws is copied out of `self` first, so no closure below holds
        // a borrow of the window while another one needs it.
        let busy = self.busy();
        let improving = self.intake.target.clone();
        let heading = match improving.as_ref() {
            Some(entry_id) => tf!(
                "cleaning.tools.watermark.chapter.reference_improve_heading",
                name = self.entry_name(entry_id)
            ),
            None => t!("cleaning.tools.watermark.chapter.reference_create_heading").to_string(),
        };
        let files: Vec<String> = self
            .intake
            .files
            .iter()
            .map(|file| file.display().to_string())
            .collect();
        let enough = self.intake.files.len() >= 2 || improving.is_some();
        let mut name = self.intake.name.clone();
        let mut run = false;
        let mut pick = false;
        let mut cancel = false;
        ui.group(|ui| {
            ui.label(heading);
            for file in &files {
                ui.small(file);
            }
            if improving.is_none() {
                ui.horizontal(|ui| {
                    ui.label(t!("cleaning.tools.watermark.chapter.reference_name_label"));
                    // The name is user data: whatever is typed is kept verbatim.
                    ui.add(
                        egui::TextEdit::singleline(&mut name)
                            .id_salt("cleaning.tools.watermark.library_intake_name")
                            .desired_width(NAME_EDIT_WIDTH),
                    );
                });
            }
            ui.horizontal(|ui| {
                let run_label = if improving.is_some() {
                    t!("cleaning.tools.watermark.chapter.reference_improve_run_button")
                } else {
                    t!("cleaning.tools.watermark.chapter.reference_run_button")
                };
                run = ui
                    .add_enabled(!busy && enough, egui::Button::new(run_label))
                    .on_disabled_hover_text(t!(
                        "cleaning.tools.watermark.chapter.reference_needs_two_error"
                    ))
                    .clicked();
                pick = ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!(
                            "cleaning.tools.watermark.chapter.reference_pick_button"
                        )),
                    )
                    .clicked();
                cancel = ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!("cleaning.common.cancel_button")),
                    )
                    .clicked();
            });
        });
        self.intake.name = name;
        if cancel {
            self.intake.reset();
            return;
        }
        if pick {
            let purpose = improving.map_or(PickerPurpose::ReferenceCrops, |entry_id| {
                PickerPurpose::ImproveEntry(entry_id)
            });
            self.start_picker(purpose);
            return;
        }
        if run {
            self.start_intake(sample_params, max_side);
        }
    }

    /// The verbatim display name of one listed entry, or its id when it is gone.
    fn entry_name(&self, entry_id: &str) -> String {
        self.entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .map_or_else(|| entry_id.to_string(), |entry| entry.name.clone())
    }

    /// Draws one entry row: preview, name editor, actions, quality verdict, calibration
    /// levels and the sources it has been used on. Returns the action the user asked for.
    fn draw_entry(&mut self, ui: &mut egui::Ui, index: usize, busy: bool) -> Option<EntryAction> {
        // Same discipline as the intake form: the row draws from locals only, so the
        // closures never hold a borrow of the window.
        let entry = self.entries.get(index)?.clone();
        // Cloning a `TextureHandle` is a refcount bump, not a copy of the image.
        let preview = self.previews.get(&entry.id).cloned();
        let mut name = self
            .edits
            .get(&entry.id)
            .cloned()
            .unwrap_or_else(|| entry.name.clone());
        let armed = self.confirm_delete.as_deref() == Some(entry.id.as_str());
        let mut action = None;
        let mut arm_delete = false;
        ui.group(|ui| {
            ui.horizontal(|ui| {
                match preview.as_ref() {
                    Some(texture) => {
                        ui.add(egui::Image::new((
                            texture.id(),
                            egui::vec2(PREVIEW_SIDE, PREVIEW_SIDE),
                        )));
                    }
                    // The template is still being decoded on a worker: hold the row's shape
                    // so it does not jump when the texture arrives.
                    None => {
                        ui.allocate_space(egui::vec2(PREVIEW_SIDE, PREVIEW_SIDE));
                    }
                }
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!busy, |ui| {
                            // The name is user data: whatever is typed is kept verbatim.
                            ui.add(
                                egui::TextEdit::singleline(&mut name)
                                    .id_salt(("cleaning.tools.watermark.library_name", index))
                                    .desired_width(NAME_EDIT_WIDTH),
                            );
                        });
                        if ui
                            .add_enabled(
                                !busy && name != entry.name,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_rename_button"
                                )),
                            )
                            .clicked()
                        {
                            action = Some(EntryAction::Rename(entry.id.clone(), name.clone()));
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.reference_improve_button"
                                )),
                            )
                            .on_hover_text(t!(
                                "cleaning.tools.watermark.chapter.reference_improve_hint"
                            ))
                            .clicked()
                        {
                            action = Some(EntryAction::Improve(entry.id.clone()));
                        }
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_export_zip_button"
                                )),
                            )
                            .clicked()
                        {
                            action = Some(EntryAction::ExportArchive(entry.id.clone()));
                        }
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(t!(
                                    "cleaning.tools.watermark.chapter.library_export_dir_button"
                                )),
                            )
                            .clicked()
                        {
                            action = Some(EntryAction::ExportFolder(entry.id.clone()));
                        }
                        let label = if armed {
                            t!("cleaning.tools.watermark.chapter.library_delete_confirm_button")
                        } else {
                            t!("cleaning.tools.watermark.chapter.library_delete_button")
                        };
                        if ui
                            .add_enabled(!busy, egui::Button::new(label))
                            .on_hover_text(t!("cleaning.tools.watermark.chapter.library_delete_hint"))
                            .clicked()
                        {
                            if armed {
                                action = Some(EntryAction::Delete(entry.id.clone()));
                            } else {
                                // Arming rather than deleting on the first click: the entry
                                // may be the only measurement of a mark that took a chapter
                                // to collect.
                                arm_delete = true;
                            }
                        }
                    });
                    draw_entry_report(ui, &entry);
                });
            });
        });
        self.edits.insert(entry.id.clone(), name);
        if arm_delete {
            self.confirm_delete = Some(entry.id);
        }
        action
    }

    /// Starts the worker one entry button asked for.
    fn run_entry_action(&mut self, action: EntryAction) {
        match action {
            EntryAction::Rename(entry_id, name) => {
                self.start_job(move |tx| {
                    let _ = tx.send(match rename_entry(&entry_id, &name) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_renamed_status",
                            name = name
                        )),
                        Err(err) => LibraryEvent::Failed(err),
                    });
                });
            }
            EntryAction::Delete(entry_id) => {
                self.confirm_delete = None;
                self.start_job(move |tx| {
                    let _ = tx.send(match delete_entry(&entry_id) {
                        Ok(()) => LibraryEvent::Changed(tf!(
                            "cleaning.tools.watermark.chapter.library_deleted_status",
                            id = entry_id
                        )),
                        Err(err) => LibraryEvent::Failed(err),
                    });
                });
            }
            EntryAction::Improve(entry_id) => {
                self.start_picker(PickerPurpose::ImproveEntry(entry_id));
            }
            EntryAction::ExportArchive(entry_id) => {
                self.start_picker(PickerPurpose::ExportArchive(entry_id));
            }
            EntryAction::ExportFolder(entry_id) => {
                self.start_picker(PickerPurpose::ExportFolder(entry_id));
            }
        }
    }
}

/// What one entry row asked the window to do. Collected during the draw and executed after
/// it, because every one of these mutates the state the row is being drawn from.
enum EntryAction {
    Rename(String, String),
    Delete(String),
    Improve(String),
    ExportArchive(String),
    ExportFolder(String),
}

/// Draws one entry's quality verdict, calibration levels and usage history.
///
/// This is the point of the whole screen: it must be readable at a glance which entries are
/// EXACT (two well-separated levels, the closed form) and which are graded — and for the
/// graded ones it names the background that would make them exact.
fn draw_entry_report(ui: &mut egui::Ui, entry: &EntrySummary) {
    let conditioning = conditioning_from_stored(&stored_calibration_of(entry));
    match conditioning.as_ref() {
        Some(ModelConditioning::Separable { .. }) => {
            ui.colored_label(
                Color32::from_rgb(
                    VERDICT_EXACT_RGB[0],
                    VERDICT_EXACT_RGB[1],
                    VERDICT_EXACT_RGB[2],
                ),
                t!("cleaning.tools.watermark.chapter.verdict_separable"),
            );
        }
        Some(ModelConditioning::DepositExact { .. }) => {
            ui.colored_label(
                Color32::from_rgb(
                    VERDICT_GRADED_RGB[0],
                    VERDICT_GRADED_RGB[1],
                    VERDICT_GRADED_RGB[2],
                ),
                t!("cleaning.tools.watermark.chapter.verdict_deposit_exact"),
            );
        }
        Some(_) | None => {
            // A verdict this build does not know is reported by its literal tag rather than
            // squeezed into the closest known wording, which would misstate its quality.
            ui.colored_label(
                Color32::from_rgb(
                    VERDICT_GRADED_RGB[0],
                    VERDICT_GRADED_RGB[1],
                    VERDICT_GRADED_RGB[2],
                ),
                tf!(
                    "cleaning.tools.watermark.chapter.library_verdict_unknown",
                    verdict = entry.verdict.clone()
                ),
            );
        }
    }
    if let Some(alpha) = entry.alpha.as_ref() {
        ui.small(tf!(
            "cleaning.tools.watermark.chapter.library_alpha_line",
            percent = format!("{:.0}", alpha.percent)
        ));
    }
    ui.small(if entry.levels.is_empty() {
        t!("cleaning.tools.watermark.chapter.levels_none").to_string()
    } else {
        tf!(
            "cleaning.tools.watermark.chapter.levels_line",
            levels = entry
                .levels
                .iter()
                .map(|level| format!("{level:.0}"))
                .collect::<Vec<_>>()
                .join(", "),
            spread = format!("{:.0}", entry.spread)
        )
    });
    if let Some(suggestion) = conditioning
        .as_ref()
        .and_then(ModelConditioning::suggested_background)
    {
        ui.small(match suggestion {
            crate::tabs::cleaning::watermark_chapter::SuggestedBackground::Darker { at_most } => {
                tf!(
                    "cleaning.tools.watermark.chapter.suggest_darker",
                    level = format!("{at_most:.0}")
                )
            }
            crate::tabs::cleaning::watermark_chapter::SuggestedBackground::Brighter {
                at_least,
            } => tf!(
                "cleaning.tools.watermark.chapter.suggest_brighter",
                level = format!("{at_least:.0}")
            ),
        });
    }
    ui.small(tf!(
        "cleaning.tools.watermark.chapter.library_entry_line",
        name = entry.id.clone(),
        width = entry.width,
        height = entry.height,
        anchors = entry.anchor_key.clone(),
        samples = entry.samples
    ));
    ui.small(if entry.sources.is_empty() {
        t!("cleaning.tools.watermark.chapter.library_sources_none").to_string()
    } else {
        tf!(
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
        )
    });
}

/// The calibration record of a listed entry, rebuilt from its summary.
///
/// The summary carries exactly the fields the verdict needs; this keeps
/// `conditioning_from_stored` the single place that knows how a tag maps back.
fn stored_calibration_of(entry: &EntrySummary) -> super::watermark_library::StoredCalibration {
    super::watermark_library::StoredCalibration {
        verdict: entry.verdict.clone(),
        levels: entry.levels.clone(),
        spread: entry.spread,
        samples: entry.samples,
        fit_method: entry.fit_method.clone(),
        clamped_pixels: 0,
        alpha: entry.alpha.clone(),
    }
}

/// Downscales a decoded template into a thumbnail `egui::ColorImage`.
fn thumbnail(template: &image::RgbaImage) -> egui::ColorImage {
    let (width, height) = template.dimensions();
    let longest = width.max(height).max(1);
    let scaled = if longest > PREVIEW_MAX_PX {
        let target_w = (width * PREVIEW_MAX_PX / longest).max(1);
        let target_h = (height * PREVIEW_MAX_PX / longest).max(1);
        image::imageops::thumbnail(template, target_w, target_h)
    } else {
        template.clone()
    };
    egui::ColorImage::from_rgba_unmultiplied(
        [scaled.width() as usize, scaled.height() as usize],
        scaled.as_raw(),
    )
}

/// Spawns the blocking native file dialog for `purpose` on a worker thread.
///
/// Every variant answers with a path list so the caller has one shape to poll; single-pick
/// dialogs answer with one element. `None` means the user cancelled.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_picker(purpose: &PickerPurpose) -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    let purpose = purpose.clone();
    thread::spawn(move || {
        let picked = match purpose {
            PickerPurpose::ReferenceCrops | PickerPurpose::ImproveEntry(_) => {
                rfd::FileDialog::new()
                    .add_filter(
                        t!("cleaning.tools.watermark.chapter.reference_files_filter"),
                        &["png", "jpg", "jpeg", "webp", "bmp"],
                    )
                    .pick_files()
            }
            PickerPurpose::ImportArchive => rfd::FileDialog::new()
                .add_filter(
                    t!("cleaning.tools.watermark.chapter.library_archive_filter"),
                    &[ENTRY_ARCHIVE_EXTENSION],
                )
                .pick_file()
                .map(|path| vec![path]),
            PickerPurpose::ImportFolder | PickerPurpose::ExportFolder(_) => {
                rfd::FileDialog::new().pick_folder().map(|path| vec![path])
            }
            PickerPurpose::ExportArchive(entry_id) => rfd::FileDialog::new()
                .set_file_name(format!("{entry_id}.{ENTRY_ARCHIVE_EXTENSION}"))
                .add_filter(
                    t!("cleaning.tools.watermark.chapter.library_archive_filter"),
                    &[ENTRY_ARCHIVE_EXTENSION],
                )
                .save_file()
                .map(|path| vec![path]),
        };
        let _ = tx.send(picked);
    });
    rx
}

/// Web fallback: the browser build has no native file dialog (`rfd` is native-only), so the
/// pick resolves immediately as cancelled and the dropped capability is logged.
#[cfg(target_arch = "wasm32")]
fn spawn_picker(_purpose: &PickerPurpose) -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    crate::runtime_log::log_warn(
        "[cleaning] watermark library file picker unavailable on web build",
    );
    let _ = tx.send(None);
    rx
}
