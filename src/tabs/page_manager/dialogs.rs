/*
File: tabs/page_manager/dialogs.rs

Purpose:
Modal dialogs of the page-manager tab: "Insert pages" (native file picker on a
worker thread), "Create blank page" (size/color/position), and the delete
confirmation with attachment counts. It also supplies the background native
picker used by the clean replacement flow.

Key structures:
- PageManagerDialog: which dialog is open, with its state.
- InsertDialogState / CreateDialogState / DeleteDialogState.
- InsertPosition: where an insert/create lands relative to the selection.

Key functions:
- PageManagerTabState::draw_dialogs(): renders the open dialog, emits actions.
- resolve_insert_at(): InsertPosition -> concrete new-order index.
- default_blank_size(): blank-page size defaults from the neighbouring page.

Notes:
The blocking `rfd` picker runs on a worker thread (GUI thread must never block);
the wasm build has no native dialog and resolves as a cancelled pick.
*/

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;
// Worker-thread spawn is only needed for the native `rfd` picker; the wasm
// build resolves the pick synchronously as cancelled.
#[cfg(not(target_arch = "wasm32"))]
use ms_thread as thread;

use crate::page_ops::PageOpKind;
use crate::widgets::WheelComboBox;
use crate::widgets::WheelSpinBox;

use super::{PageManagerAction, PageManagerTabState};

/// Default blank-page size when the project is empty or no neighbour size is
/// known (useful for translator title/credits pages).
const DEFAULT_BLANK_WIDTH_PX: u32 = 1000;
const DEFAULT_BLANK_HEIGHT_PX: u32 = 1500;

/// Bounds for the blank-page size spin boxes.
const BLANK_SIZE_MIN_PX: u32 = 1;
const BLANK_SIZE_MAX_PX: u32 = 65535;

/// On-disk trash directory name used by the page-ops engine inside the chapter
/// folder. Persistence identifier shown to the user only through a placeholder
/// (i18n-exempt literal, see `docs/i18n_exclusions.md`).
pub(super) const PAGE_OP_TRASH_DIR: &str = ".pageop_trash";

/// Where an inserted/created page lands relative to the current selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertPosition {
    /// Before the first selected page.
    BeforeSelection,
    /// After the last selected page.
    AfterSelection,
    /// At the very end of the chapter.
    AtEnd,
}

impl InsertPosition {
    /// Localized display label of this option.
    fn label(self) -> &'static str {
        match self {
            InsertPosition::BeforeSelection => t!("page_manager.insert_dialog.pos_before_selection"),
            InsertPosition::AfterSelection => t!("page_manager.insert_dialog.pos_after_selection"),
            InsertPosition::AtEnd => t!("page_manager.insert_dialog.pos_at_end"),
        }
    }
}

/// Converts an [`InsertPosition`] into the concrete `at` index of the new page
/// order (see `PageOpKind::InsertFiles`/`CreateBlank`). With an empty selection
/// the selection-relative options degrade to "at the end".
pub(super) fn resolve_insert_at(
    position: InsertPosition,
    selection: &BTreeSet<usize>,
    page_count: usize,
) -> usize {
    match position {
        InsertPosition::BeforeSelection => selection.first().copied().unwrap_or(page_count),
        InsertPosition::AfterSelection => selection
            .last()
            .map(|idx| idx.saturating_add(1))
            .unwrap_or(page_count),
        InsertPosition::AtEnd => page_count,
    }
}

/// Picks the default size for a new blank page inserted at new-order index `at`:
/// the PREVIOUS page relative to the insert position, else the FOLLOWING page,
/// else 1000x1500 (empty project / no known sizes). `size_of` returns the pixel
/// size of a current page index when known.
pub(super) fn default_blank_size(
    size_of: &dyn Fn(usize) -> Option<(u32, u32)>,
    at: usize,
    page_count: usize,
) -> (u32, u32) {
    if page_count == 0 {
        return (DEFAULT_BLANK_WIDTH_PX, DEFAULT_BLANK_HEIGHT_PX);
    }
    // Previous page relative to the insert slot; `at` may equal page_count
    // (insert at end), in which case the previous page is the current last one.
    let prev = at.checked_sub(1).filter(|idx| *idx < page_count);
    // The page currently occupying the slot becomes the FOLLOWING page after insert.
    let next = (at < page_count).then_some(at);
    prev.and_then(size_of)
        .or_else(|| next.and_then(size_of))
        .unwrap_or((DEFAULT_BLANK_WIDTH_PX, DEFAULT_BLANK_HEIGHT_PX))
}

/// State of the "Insert pages" dialog.
pub(super) struct InsertDialogState {
    position: InsertPosition,
    /// Receiver of the file-picker worker while a pick is in progress.
    picker_rx: Option<Receiver<Option<Vec<PathBuf>>>>,
}

/// State of the "Create blank page" dialog.
pub(super) struct CreateDialogState {
    position: InsertPosition,
    width: u32,
    height: u32,
    /// Background fill, sRGB (alpha is always opaque for a page).
    color: [u8; 3],
    /// `(at, size)` of the last applied default so switching the insert position
    /// re-seeds the size only while the user has not edited it.
    seeded: Option<(usize, (u32, u32))>,
}

/// State of the delete confirmation dialog: the sorted, deduplicated current
/// page indices staged for deletion.
pub(super) struct DeleteDialogState {
    indices: Vec<usize>,
}

/// The dialog currently open in the page-manager tab, if any.
pub(super) enum PageManagerDialog {
    Insert(InsertDialogState),
    Create(CreateDialogState),
    Delete(DeleteDialogState),
}

impl PageManagerDialog {
    /// Fresh "Insert pages" dialog; defaults to inserting after the selection
    /// when one exists, else at the end.
    pub(super) fn insert(has_selection: bool) -> Self {
        PageManagerDialog::Insert(InsertDialogState {
            position: if has_selection {
                InsertPosition::AfterSelection
            } else {
                InsertPosition::AtEnd
            },
            picker_rx: None,
        })
    }

    /// Fresh "Create blank page" dialog (size is seeded on first draw).
    pub(super) fn create(has_selection: bool) -> Self {
        PageManagerDialog::Create(CreateDialogState {
            position: if has_selection {
                InsertPosition::AfterSelection
            } else {
                InsertPosition::AtEnd
            },
            width: DEFAULT_BLANK_WIDTH_PX,
            height: DEFAULT_BLANK_HEIGHT_PX,
            color: [255, 255, 255],
            seeded: None,
        })
    }

    /// Fresh delete confirmation for the given current page indices.
    pub(super) fn delete(indices: Vec<usize>) -> Self {
        PageManagerDialog::Delete(DeleteDialogState { indices })
    }

    /// Whether a background file pick is currently pending.
    pub(super) fn picker_active(&self) -> bool {
        match self {
            PageManagerDialog::Insert(state) => state.picker_rx.is_some(),
            PageManagerDialog::Create(_) | PageManagerDialog::Delete(_) => false,
        }
    }
}

/// Spawns the blocking native multi-file picker on a worker thread and returns
/// the receiver of its result (`None` = cancelled). The extension filter matches
/// `project::collect_images` (png/jpg/jpeg).
#[cfg(not(target_arch = "wasm32"))]
fn spawn_files_picker() -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    thread::spawn(move || {
        let files = rfd::FileDialog::new()
            .add_filter(
                t!("page_manager.insert_dialog.images_filter"),
                &["png", "jpg", "jpeg"],
            )
            .pick_files();
        let _ = tx.send(files);
    });
    rx
}

/// Starts the single-image picker used for replacement clean layers.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_clean_picker() -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    thread::spawn(move || {
        let file = rfd::FileDialog::new()
            .add_filter(t!("page_manager.clean_picker_filter"), &["png", "jpg", "jpeg"])
            .pick_file();
        let _ = tx.send(file);
    });
    rx
}

/// Web fallback for the replacement-clean picker.
#[cfg(target_arch = "wasm32")]
pub(super) fn spawn_clean_picker() -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    crate::runtime_log::log_warn("[page_manager] native clean picker unavailable on web build");
    let _ = tx.send(None);
    rx
}

/// Web stub: the browser build has no native file dialog (`rfd`), so the pick
/// resolves immediately as cancelled and the dropped capability is logged.
#[cfg(target_arch = "wasm32")]
fn spawn_files_picker() -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = mpsc::channel::<Option<Vec<PathBuf>>>();
    crate::runtime_log::log_warn("[page_manager] native file picker unavailable on web build");
    let _ = tx.send(None);
    rx
}

/// Draws the position combo shared by the insert and create dialogs. The
/// selection-relative options are offered only while a selection exists.
fn draw_position_combo(
    ui: &mut egui::Ui,
    id_salt: &'static str,
    position: &mut InsertPosition,
    has_selection: bool,
) {
    if !has_selection
        && matches!(
            position,
            InsertPosition::BeforeSelection | InsertPosition::AfterSelection
        )
    {
        *position = InsertPosition::AtEnd;
    }
    ui.horizontal(|ui| {
        ui.label(t!("page_manager.insert_dialog.position_label"));
        WheelComboBox::from_id_salt(id_salt)
            .selected_text(position.label())
            .show_ui(ui, |ui| {
                if has_selection {
                    ui.selectable_value(
                        position,
                        InsertPosition::BeforeSelection,
                        InsertPosition::BeforeSelection.label(),
                    );
                    ui.selectable_value(
                        position,
                        InsertPosition::AfterSelection,
                        InsertPosition::AfterSelection.label(),
                    );
                }
                ui.selectable_value(
                    position,
                    InsertPosition::AtEnd,
                    InsertPosition::AtEnd.label(),
                );
            });
    });
}

impl PageManagerTabState {
    /// Renders the currently open dialog (if any) and pushes the resulting
    /// structural-op actions into `actions`. `page_count` is the current number
    /// of pages; `size_of` resolves a page's pixel size for the create-dialog
    /// defaults.
    pub(super) fn draw_dialogs(
        &mut self,
        ctx: &egui::Context,
        page_count: usize,
        size_of: &dyn Fn(usize) -> Option<(u32, u32)>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) {
        // Take the dialog out so its mutable state does not alias the rest of
        // `self` (selection, badge caches) borrowed by the draw helpers below.
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let kept = match dialog {
            PageManagerDialog::Insert(state) => self
                .draw_insert_dialog(ctx, state, page_count, op_in_progress, actions)
                .map(PageManagerDialog::Insert),
            PageManagerDialog::Create(state) => self
                .draw_create_dialog(ctx, state, page_count, size_of, op_in_progress, actions)
                .map(PageManagerDialog::Create),
            PageManagerDialog::Delete(state) => self
                .draw_delete_dialog(ctx, state, op_in_progress, actions)
                .map(PageManagerDialog::Delete),
        };
        self.dialog = kept;
    }

    /// Draws the "Insert pages" dialog. Returns the state to keep, or `None`
    /// when the dialog closed this frame.
    fn draw_insert_dialog(
        &mut self,
        ctx: &egui::Context,
        mut state: InsertDialogState,
        page_count: usize,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<InsertDialogState> {
        // Poll a pending pick first: a completed pick emits the op and closes.
        if let Some(rx) = state.picker_rx.as_ref() {
            match rx.try_recv() {
                Ok(Some(files)) if !files.is_empty() => {
                    let at = resolve_insert_at(state.position, &self.selection, page_count);
                    actions.push(PageManagerAction::RequestOp(PageOpKind::InsertFiles {
                        at,
                        files,
                    }));
                    return None;
                }
                Ok(_) => {
                    // Cancelled or empty pick: stay open, allow another attempt.
                    state.picker_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.picker_rx = None;
                    self.error_message =
                        Some(t!("page_manager.insert_dialog.picker_failed_error").to_string());
                }
            }
        }

        let picking = state.picker_rx.is_some();
        let has_selection = !self.selection.is_empty();
        let mut keep_open = true;
        let mut close_clicked = false;
        egui::Window::new(t!("page_manager.insert_dialog.title"))
            .id(egui::Id::new("page_manager_insert_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                draw_position_combo(
                    ui,
                    "page_manager_insert_dialog_position",
                    &mut state.position,
                    has_selection,
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !op_in_progress && !picking,
                            egui::Button::new(t!("page_manager.insert_dialog.pick_files_button")),
                        )
                        .clicked()
                    {
                        state.picker_rx = Some(spawn_files_picker());
                    }
                    if picking {
                        ui.spinner();
                        ui.label(t!("page_manager.insert_dialog.picking_status"));
                    }
                });
                ui.add_space(8.0);
                if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
                    close_clicked = true;
                }
            });
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }

    /// Draws the "Create blank page" dialog. Returns the state to keep, or
    /// `None` when the dialog closed this frame.
    fn draw_create_dialog(
        &mut self,
        ctx: &egui::Context,
        mut state: CreateDialogState,
        page_count: usize,
        size_of: &dyn Fn(usize) -> Option<(u32, u32)>,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<CreateDialogState> {
        let has_selection = !self.selection.is_empty();
        let at = resolve_insert_at(state.position, &self.selection, page_count);
        // Re-seed the default size when the target slot changes, but never
        // overwrite a size the user already edited away from the previous seed.
        let user_edited = state
            .seeded
            .is_some_and(|(_, size)| size != (state.width, state.height));
        if state.seeded.map(|(seeded_at, _)| seeded_at) != Some(at) && !user_edited {
            let (width, height) = default_blank_size(size_of, at, page_count);
            state.width = width;
            state.height = height;
            state.seeded = Some((at, (width, height)));
        }

        let mut keep_open = true;
        let mut close_clicked = false;
        let mut create_clicked = false;
        egui::Window::new(t!("page_manager.create_dialog.title"))
            .id(egui::Id::new("page_manager_create_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                draw_position_combo(
                    ui,
                    "page_manager_create_dialog_position",
                    &mut state.position,
                    has_selection,
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(t!("page_manager.create_dialog.width_label"));
                    ui.add(
                        WheelSpinBox::new(&mut state.width)
                            .range(BLANK_SIZE_MIN_PX..=BLANK_SIZE_MAX_PX),
                    );
                    ui.add_space(8.0);
                    ui.label(t!("page_manager.create_dialog.height_label"));
                    ui.add(
                        WheelSpinBox::new(&mut state.height)
                            .range(BLANK_SIZE_MIN_PX..=BLANK_SIZE_MAX_PX),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(t!("page_manager.create_dialog.color_label"));
                    ui.color_edit_button_srgb(&mut state.color);
                    if ui
                        .button(t!("page_manager.create_dialog.preset_white_button"))
                        .clicked()
                    {
                        state.color = [255, 255, 255];
                    }
                    if ui
                        .button(t!("page_manager.create_dialog.preset_black_button"))
                        .clicked()
                    {
                        state.color = [0, 0, 0];
                    }
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !op_in_progress,
                            egui::Button::new(t!("page_manager.create_dialog.create_button")),
                        )
                        .clicked()
                    {
                        create_clicked = true;
                    }
                    if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
                        close_clicked = true;
                    }
                });
            });
        if create_clicked {
            actions.push(PageManagerAction::RequestOp(PageOpKind::CreateBlank {
                at,
                width: state.width,
                height: state.height,
                rgba: [state.color[0], state.color[1], state.color[2], 255],
            }));
            return None;
        }
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }

    /// Draws the delete confirmation dialog listing the staged pages and what is
    /// attached to them. Returns the state to keep, or `None` when closed.
    fn draw_delete_dialog(
        &mut self,
        ctx: &egui::Context,
        state: DeleteDialogState,
        op_in_progress: bool,
        actions: &mut Vec<PageManagerAction>,
    ) -> Option<DeleteDialogState> {
        // Human-readable 1-based page numbers, e.g. "1, 3, 7".
        let pages_list = state
            .indices
            .iter()
            .map(|idx| (idx + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let bubbles: usize = state
            .indices
            .iter()
            .map(|idx| self.bubble_counts.get(idx).copied().unwrap_or(0))
            .sum();
        let layers: usize = state
            .indices
            .iter()
            .map(|idx| self.effective_layer_count(*idx))
            .sum();
        let cleaned = state
            .indices
            .iter()
            .filter(|idx| self.clean_present.get(**idx).copied().unwrap_or(false))
            .count();

        let mut keep_open = true;
        let mut close_clicked = false;
        let mut confirm_clicked = false;
        egui::Window::new(t!("page_manager.delete_dialog.title"))
            .id(egui::Id::new("page_manager_delete_dialog"))
            .open(&mut keep_open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.add(
                    egui::Label::new(tf!("page_manager.delete_dialog.message", pages = pages_list))
                        .wrap(),
                );
                ui.add_space(4.0);
                ui.label(tf!(
                    "page_manager.delete_dialog.attachments",
                    bubbles = bubbles,
                    layers = layers,
                    cleaned = cleaned
                ));
                ui.add_space(6.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    t!("page_manager.delete_dialog.apply_warning"),
                );
                ui.add(
                    egui::Label::new(tf!(
                        "page_manager.delete_dialog.trash_note",
                        dir = PAGE_OP_TRASH_DIR
                    ))
                    .wrap(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !op_in_progress,
                            egui::Button::new(t!("page_manager.delete_dialog.confirm_button")),
                        )
                        .clicked()
                    {
                        confirm_clicked = true;
                    }
                    if ui.button(t!("page_manager.dialog.cancel_button")).clicked() {
                        close_clicked = true;
                    }
                });
            });
        if confirm_clicked {
            actions.push(PageManagerAction::RequestOp(PageOpKind::Delete {
                indices: state.indices,
            }));
            return None;
        }
        if !keep_open || close_clicked {
            return None;
        }
        Some(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sizes(pairs: &[(usize, (u32, u32))]) -> HashMap<usize, (u32, u32)> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn resolve_insert_at_covers_selection_and_end() {
        let selection: BTreeSet<usize> = [2, 5].into_iter().collect();
        assert_eq!(
            resolve_insert_at(InsertPosition::BeforeSelection, &selection, 10),
            2
        );
        assert_eq!(
            resolve_insert_at(InsertPosition::AfterSelection, &selection, 10),
            6
        );
        assert_eq!(resolve_insert_at(InsertPosition::AtEnd, &selection, 10), 10);
    }

    #[test]
    fn resolve_insert_at_empty_selection_degrades_to_end() {
        let selection = BTreeSet::new();
        assert_eq!(
            resolve_insert_at(InsertPosition::BeforeSelection, &selection, 4),
            4
        );
        assert_eq!(
            resolve_insert_at(InsertPosition::AfterSelection, &selection, 4),
            4
        );
    }

    #[test]
    fn default_blank_size_empty_project_uses_builtin_default() {
        let map = sizes(&[]);
        let lookup = |idx: usize| map.get(&idx).copied();
        assert_eq!(default_blank_size(&lookup, 0, 0), (1000, 1500));
    }

    #[test]
    fn default_blank_size_prefers_previous_page() {
        let map = sizes(&[(1, (800, 1200)), (2, (900, 1300))]);
        let lookup = |idx: usize| map.get(&idx).copied();
        // Inserting at index 2: previous page is index 1.
        assert_eq!(default_blank_size(&lookup, 2, 3), (800, 1200));
    }

    #[test]
    fn default_blank_size_at_start_uses_following_page() {
        let map = sizes(&[(0, (700, 1100))]);
        let lookup = |idx: usize| map.get(&idx).copied();
        // Inserting at index 0: no previous page, the current first page follows.
        assert_eq!(default_blank_size(&lookup, 0, 2), (700, 1100));
    }

    #[test]
    fn default_blank_size_unknown_previous_falls_back_to_next() {
        let map = sizes(&[(2, (640, 960))]);
        let lookup = |idx: usize| map.get(&idx).copied();
        // Previous page (idx 1) has no known size yet; the following page does.
        assert_eq!(default_blank_size(&lookup, 2, 3), (640, 960));
    }

    #[test]
    fn default_blank_size_no_known_sizes_uses_builtin_default() {
        let map = sizes(&[]);
        let lookup = |idx: usize| map.get(&idx).copied();
        assert_eq!(default_blank_size(&lookup, 1, 3), (1000, 1500));
    }
}
