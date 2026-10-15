/*
File: settings/typesetting/hybrid_fonts.rs

Purpose:
The "Кастомные шрифты" section of the settings "Настройки шрифтов" block and the custom-font
parts of the per-font properties window (`dev-docs/hybrid_fonts_plan.md` §3.4, decisions D6/D7).
UI ONLY: saved custom fonts (recipes, e.g. hybrids) live in the typing model and are reached
exclusively through `crate::tabs::typing::font_admin`; the editor body is the shared
`font_admin::draw_hybrid_editor`.

Main responsibilities:
- list every SAVED custom font (`font_admin::CustomFontRow`): an available one is a button
  drawn in its OWN compiled typeface that opens the properties window; an unavailable one
  (missing source, kind this build cannot compile, damaged recipe) is greyed with its
  localized reason and can only be deleted;
- delete a saved font with the inline two-step confirm (the `font_groups.rs` pattern: arm,
  confirm with a second single click, auto-disarm when the pointer leaves);
- own the «+ Создать гибридный шрифт» dialog (`egui::Window`, pinned `Id`): a name field, the
  shared editor with a host-drawn commit («Сохранить» compiles the full face, then
  `font_admin::create_custom_font` stores it under the validated name);
- give the properties window of a saved font its rename editor (`font_admin::rename_custom_font`)
  and its «Редактирование гибрида» section (the shared editor, «Применить» ->
  `font_admin::update_custom_font_recipe`).

Key types:
- `HybridFontsSectionState` (owned by `FontSettingsEditorState`)
- `CustomFontProperties` (owned by `FontPropertiesState` for a saved custom font)
- `CustomFontNameError` / `DeleteStep`

Key functions:
- `validate_custom_font_name` (pure: blank / case-insensitive clash, unit-tested)
- `delete_step` (pure two-step delete transition, unit-tested)
- `custom_row_name` (pure row naming, unit-tested)

Notes:
- Names: a custom font's name must differ, case-insensitively, from every other custom font
  AND from every real font's display label, so the font lists never show two entries the user
  cannot tell apart. The store checks only the custom half (it cannot see font files).
- Threading: nothing here reads a file or compiles. The editor runs its previews and the full
  compile on its own workers; the store mutations are in-memory and persist off-thread.
- The section's rows draw a saved font in its compiled face only once that face is in the
  compile service's cache (`FontPreviewSource::request_family` never compiles); until then the
  interface font is used.
*/

use crate::runtime_log;
use crate::tabs::typing::font_admin::{
    self, CommitButton, CustomFontRow, CustomFontStoreError, FontEntry, HybridEditorCx, HybridEditorOutcome,
    HybridEditorState, SourceCatalog,
};

use super::font_settings::{FontNameDisplayMode, FontSettingsEditorState, PREVIEW_ROW_HEIGHT_FACTOR};

/// Pinned id of the create dialog: its title is localized (`05-ids-and-i18n.md` §2).
const CREATE_WINDOW_ID: &str = "typing.font_settings.hybrid_create_window";
/// Rows of the custom-font list kept visible before it scrolls.
const LIST_VISIBLE_ROWS: f32 = 8.0;
/// Width of the name fields (create dialog and rename), in points.
const NAME_EDIT_WIDTH: f32 = 280.0;

/// Why a proposed custom-font name is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CustomFontNameError {
    /// Empty or whitespace-only.
    Blank,
    /// Equal (trimmed, case-insensitively) to another custom font's name or to a real font's
    /// display label.
    Taken,
}

impl CustomFontNameError {
    /// The localized message shown under the name field. GUI thread (`t!`).
    fn message(self) -> String {
        match self {
            Self::Blank => t!("typing.font_settings.hybrid_name_empty_error").to_string(),
            Self::Taken => t!("typing.font_settings.hybrid_name_taken_error").to_string(),
        }
    }
}

/// Validates a proposed custom-font name against the names it must not repeat.
///
/// `taken` holds every name the new one must differ from: the other custom fonts' names and the
/// real fonts' display labels (the caller excludes the font being renamed). The comparison is
/// trimmed and Unicode case-insensitive (names are often Cyrillic). Returns the trimmed name.
///
/// # Errors
/// [`CustomFontNameError::Blank`] for an empty name, [`CustomFontNameError::Taken`] for a clash.
pub(super) fn validate_custom_font_name(name: &str, taken: &[&str]) -> Result<String, CustomFontNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CustomFontNameError::Blank);
    }
    let folded = name.to_lowercase();
    if taken.iter().any(|other| other.trim().to_lowercase() == folded) {
        return Err(CustomFontNameError::Taken);
    }
    Ok(name.to_string())
}

/// The localized message for a refused store mutation. The name errors reuse the name-field
/// texts (a name clash can still reach the store when another surface saved the same name in
/// between); every other refusal is a generic "could not save" — the detail goes to the log.
fn store_error_message(err: &CustomFontStoreError) -> String {
    match err {
        CustomFontStoreError::BlankName => CustomFontNameError::Blank.message(),
        CustomFontStoreError::NameTaken => CustomFontNameError::Taken.message(),
        CustomFontStoreError::UnknownIdentity
        | CustomFontStoreError::Invalid(_)
        | CustomFontStoreError::Unsupported
        | CustomFontStoreError::IdentityCollision => t!("typing.font_settings.custom_font_save_failed_error").to_string(),
    }
}

/// One frame's transition of the inline two-step delete control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeleteStep {
    /// Nothing changes.
    Stay,
    /// The first click: this row becomes the armed one.
    Arm,
    /// The pointer left the armed button: disarm, so a later click elsewhere cannot confirm.
    Disarm,
    /// A second single click on the armed button: delete now.
    Confirm,
}

/// Decides the delete control's transition from this frame's response.
///
/// A physical DOUBLE-click never confirms (its two presses would arm and confirm in one
/// gesture), and an armed button the pointer no longer hovers disarms.
#[must_use]
pub(super) fn delete_step(armed: bool, clicked: bool, double_clicked: bool, hovered: bool) -> DeleteStep {
    if clicked && !double_clicked {
        if armed { DeleteStep::Confirm } else { DeleteStep::Arm }
    } else if armed && !hovered {
        DeleteStep::Disarm
    } else {
        DeleteStep::Stay
    }
}

/// The name a custom-font row shows: the stored name, or the identity when the document holds
/// a blank one (a hand-edited or damaged entry), so a row is never drawn empty.
#[must_use]
pub(super) fn custom_row_name(row: &CustomFontRow) -> String {
    let name = row.name.trim();
    if name.is_empty() { row.identity.clone() } else { name.to_string() }
}

/// State of the "Кастомные шрифты" section: the armed delete and the open create dialog.
#[derive(Debug, Default)]
pub(super) struct HybridFontsSectionState {
    /// Identity of the row armed for the two-step delete (`None` = disarmed).
    delete_armed: Option<String>,
    /// The open «Новый гибридный шрифт» dialog, if any (at most one).
    create: Option<CreateDialog>,
}

impl HybridFontsSectionState {
    /// Draws the section body: the saved custom fonts (available ones open the properties
    /// window on click) and the «+ Создать гибридный шрифт» button. Returns the clicked font.
    pub(super) fn draw_section(&mut self, ui: &mut egui::Ui, rows: &[CustomFontRow]) -> Option<FontEntry> {
        // A font deleted elsewhere (or just now) must not leave its arm behind.
        if let Some(armed) = &self.delete_armed
            && !rows.iter().any(|row| &row.identity == armed)
        {
            self.delete_armed = None;
        }
        let mut clicked = None;
        if rows.is_empty() {
            ui.small(t!("typing.font_settings.custom_fonts_empty_hint"));
        } else {
            let row_height = egui::TextStyle::Body.resolve(ui.style()).size * PREVIEW_ROW_HEIGHT_FACTOR;
            egui::ScrollArea::vertical()
                .id_salt("font_settings_custom_rows")
                .max_height(row_height * LIST_VISIBLE_ROWS)
                .auto_shrink([false, true])
                .show_rows(ui, row_height, rows.len(), |ui, range| {
                    for index in range {
                        let Some(row) = rows.get(index) else {
                            continue;
                        };
                        ui.horizontal(|ui| {
                            self.draw_delete_control(ui, row);
                            match &row.font {
                                // The list has no name switch: a custom font's identity is a
                                // minted key, its name is the only meaningful one.
                                Some(font) => {
                                    if FontSettingsEditorState::draw_font_name_row(ui, font, FontNameDisplayMode::Custom) {
                                        clicked = Some(font.clone());
                                    }
                                }
                                None => draw_unavailable_row(ui, row),
                            }
                        });
                    }
                });
        }
        ui.add_space(4.0);
        if ui.button(t!("typing.font_settings.create_hybrid_font_button")).clicked() && self.create.is_none() {
            self.create = Some(CreateDialog::new());
        }
        clicked
    }

    /// Whether the create dialog is open (the caller builds its name list only then).
    #[must_use]
    pub(super) fn is_create_dialog_open(&self) -> bool {
        self.create.is_some()
    }

    /// Draws the inline two-step delete button of one row (see [`delete_step`]).
    fn draw_delete_control(&mut self, ui: &mut egui::Ui, row: &CustomFontRow) {
        let armed = self.delete_armed.as_deref() == Some(row.identity.as_str());
        let button = if armed {
            egui::Button::new(t!("typing.font_settings.custom_font_delete_confirm_button"))
                .fill(ms_theme::status::DESTRUCTIVE_ARMED_FILL)
        } else {
            egui::Button::new(t!("typing.font_settings.custom_font_delete_button"))
        };
        let response = ui.add(button).on_hover_text(t!("typing.font_settings.custom_font_delete_warning"));
        match delete_step(armed, response.clicked(), response.double_clicked(), response.hovered()) {
            DeleteStep::Stay => {}
            DeleteStep::Arm => self.delete_armed = Some(row.identity.clone()),
            DeleteStep::Disarm => self.delete_armed = None,
            DeleteStep::Confirm => {
                self.delete_armed = None;
                if font_admin::delete_custom_font(&row.identity) {
                    runtime_log::log_info(format!(
                        "[settings] deleted custom font '{}' ({})",
                        custom_row_name(row),
                        row.identity
                    ));
                } else {
                    runtime_log::log_warn(format!(
                        "[settings] custom font '{}' ({}) was already gone when its delete was confirmed",
                        custom_row_name(row),
                        row.identity
                    ));
                }
            }
        }
    }

    /// Draws the create dialog when open (a top-level window, never nested in another
    /// window's body). `catalog` is the editor's source catalog (`None` while the font lists
    /// load); `taken` lists every name a new custom font must not repeat.
    pub(super) fn draw_create_dialog(&mut self, ctx: &egui::Context, catalog: Option<&SourceCatalog>, taken: &[&str]) {
        let Some(mut dialog) = self.create.take() else {
            return;
        };
        let mut window_open = true;
        let mut close = false;
        egui::Window::new(t!("typing.font_settings.hybrid_create_window_title"))
            .id(egui::Id::new(CREATE_WINDOW_ID))
            .open(&mut window_open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            // The editor (rows, coverage, three preview strips) can outgrow the screen.
            .vscroll(true)
            .show(ctx, |ui| {
                close = dialog.draw(ui, catalog, taken);
            });
        if window_open && !close {
            self.create = Some(dialog);
        } else {
            // Closing is a cancel: dropping the dialog drops its editor, whose `Drop` stops a
            // build nobody will save.
            drop(dialog);
        }
    }
}

/// Draws an unavailable saved font: its name greyed plus the localized reason, which the hover
/// repeats (the small line may be cut by the row width).
fn draw_unavailable_row(ui: &mut egui::Ui, row: &CustomFontRow) {
    let name = ui.weak(custom_row_name(row));
    // `unavailable` is `Some` exactly when `font` is `None`; without a reason the greyed name
    // alone is still truthful.
    if let Some(reason) = row.unavailable.as_ref().map(font_admin::CustomFontUnavailability::tooltip) {
        name.on_hover_text(reason.as_str());
        ui.small(reason);
    }
}

/// The «Новый гибридный шрифт» dialog.
#[derive(Debug)]
struct CreateDialog {
    /// The name field.
    name: String,
    /// The shared editor (host-drawn commit).
    editor: HybridEditorState,
    /// The validated name the running build will be saved under (set by «Сохранить»).
    pending_name: Option<String>,
    /// Localized error under the name field (validation or a refused save).
    error: Option<String>,
}

impl CreateDialog {
    /// An empty dialog: blank name, a new draft.
    fn new() -> Self {
        Self {
            name: String::new(),
            editor: HybridEditorState::new_draft(egui::Id::new("typing.font_settings.hybrid_create_editor")),
            pending_name: None,
            error: None,
        }
    }

    /// Draws one frame of the dialog body; returns whether the dialog should close (saved or
    /// cancelled).
    fn draw(&mut self, ui: &mut egui::Ui, catalog: Option<&SourceCatalog>, taken: &[&str]) -> bool {
        let committing = self.editor.is_committing();
        ui.horizontal(|ui| {
            ui.label(t!("typing.font_settings.hybrid_name_label"));
            // Frozen while building: the name was validated when the build started.
            ui.add_enabled(
                !committing,
                egui::TextEdit::singleline(&mut self.name)
                    .id_salt("typing.font_settings.hybrid_name_label")
                    .desired_width(NAME_EDIT_WIDTH),
            );
        });
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error.as_str());
        }
        ui.separator();
        let mut close = false;
        let Some(catalog) = catalog else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t!("typing.font_settings.loading_status"));
            });
            if ui.button(t!("typing.font_settings.hybrid_cancel_button")).clicked() {
                close = true;
            }
            return close;
        };
        let outcome = font_admin::draw_hybrid_editor(ui, &mut self.editor, &HybridEditorCx { catalog, commit: CommitButton::HostDrawn });
        match outcome {
            HybridEditorOutcome::Idle => {}
            HybridEditorOutcome::Committed(committed) => {
                if let Some(name) = self.pending_name.take() {
                    close = self.save_committed(&name, &committed.recipe);
                }
            }
            // The editor shows the failure line itself; the pending save is abandoned.
            HybridEditorOutcome::CommitFailed => self.pending_name = None,
        }
        ui.separator();
        ui.horizontal(|ui| {
            let can_save = !self.editor.is_committing() && self.editor.draft_recipe().is_ok();
            if ui.add_enabled(can_save, egui::Button::new(t!("typing.font_settings.hybrid_save_button"))).clicked() {
                self.start_save(catalog, taken);
            }
            if ui.button(t!("typing.font_settings.hybrid_cancel_button")).clicked() {
                close = true;
            }
        });
        close
    }

    /// «Сохранить»: validates the name, then starts the full build; the font is stored when
    /// the build reports `Committed`.
    fn start_save(&mut self, catalog: &SourceCatalog, taken: &[&str]) {
        let name = match validate_custom_font_name(&self.name, taken) {
            Ok(name) => name,
            Err(err) => {
                self.error = Some(err.message());
                return;
            }
        };
        self.error = None;
        match self.editor.start_commit(catalog) {
            Ok(()) => self.pending_name = Some(name),
            // The button is enabled only for a valid draft; a refusal is an edit racing the
            // click in one frame and leaves nothing started.
            Err(err) => runtime_log::log_warn(format!("[settings] hybrid font save refused: {err}")),
        }
    }

    /// Stores the built recipe under `name`. Returns whether the dialog may close; a refusal
    /// keeps it open with a localized message.
    fn save_committed(&mut self, name: &str, recipe: &font_admin::CustomFontRecipe) -> bool {
        match font_admin::create_custom_font(name, recipe.clone()) {
            Ok(identity) => {
                runtime_log::log_info(format!("[settings] saved hybrid font '{name}' as {identity}"));
                true
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[settings] could not save hybrid font '{name}': {err}; recipe={recipe:?}"
                ));
                self.error = Some(store_error_message(&err));
                false
            }
        }
    }
}

/// The custom-font parts of the properties window of one SAVED custom font: the rename editor
/// («Подробности») and the hybrid editor («Редактирование гибрида»).
#[derive(Debug)]
pub(super) struct CustomFontProperties {
    /// The saved identity (`MSCustom-<16 hex>`), the key of every mutation here.
    identity: String,
    /// Rename buffer, prefilled with the stored name.
    name_buf: String,
    /// Localized rename error.
    rename_error: Option<String>,
    /// The shared editor opened on the stored recipe; `None` when the store holds no recipe
    /// this build can edit (the entry vanished, or is unsupported).
    editor: Option<HybridEditorState>,
    /// Localized error of the last «Применить».
    apply_error: Option<String>,
}

impl CustomFontProperties {
    /// Opens the parts for the saved font `identity` named `name`, reading its recipe from the
    /// store (in-memory; GUI-thread safe).
    #[must_use]
    pub(super) fn open(identity: &str, name: &str) -> Self {
        let key = font_admin::normalize_font_identity(identity);
        let recipe = font_admin::list_custom_fonts()
            .into_iter()
            .find(|info| font_admin::normalize_font_identity(&info.identity) == key)
            .and_then(|info| info.recipe);
        if recipe.is_none() {
            runtime_log::log_warn(format!(
                "[settings] custom font {identity} has no editable recipe; its hybrid section stays empty"
            ));
        }
        Self {
            identity: identity.to_string(),
            name_buf: name.to_string(),
            rename_error: None,
            editor: recipe.map(|recipe| {
                HybridEditorState::from_recipe(&recipe, egui::Id::new(("typing.font_settings.custom_hybrid_editor", identity)))
            }),
            apply_error: None,
        }
    }

    /// The saved identity these parts edit.
    #[must_use]
    pub(super) fn identity(&self) -> &str {
        &self.identity
    }

    /// Draws the rename editor: the name field plus «Применить» (Enter applies too).
    /// `real_names` are the real fonts' display labels, which a custom name must not repeat.
    pub(super) fn draw_rename(&mut self, ui: &mut egui::Ui, real_names: &[String]) {
        let (response, apply_clicked) = ui
            .horizontal(|ui| {
                ui.label(t!("typing.font_settings.hybrid_name_label"));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.name_buf)
                        .id_salt("typing.font_settings.custom_font_rename_edit")
                        .desired_width(NAME_EDIT_WIDTH),
                );
                (response, ui.button(t!("typing.font_settings.properties_apply_button")).clicked())
            })
            .inner;
        let submitted = response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if submitted || apply_clicked {
            self.apply_rename(real_names);
        }
        if let Some(error) = &self.rename_error {
            ui.colored_label(ui.visuals().error_fg_color, error.as_str());
        }
    }

    /// Validates and stores the rename buffer.
    fn apply_rename(&mut self, real_names: &[String]) {
        let key = font_admin::normalize_font_identity(&self.identity);
        let custom_names: Vec<String> = font_admin::list_custom_fonts()
            .into_iter()
            .filter(|info| font_admin::normalize_font_identity(&info.identity) != key)
            .map(|info| info.name)
            .collect();
        let taken: Vec<&str> = custom_names.iter().chain(real_names).map(String::as_str).collect();
        let name = match validate_custom_font_name(&self.name_buf, &taken) {
            Ok(name) => name,
            Err(err) => {
                self.rename_error = Some(err.message());
                return;
            }
        };
        match font_admin::rename_custom_font(&self.identity, &name) {
            Ok(()) => {
                self.rename_error = None;
                self.name_buf = name;
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[settings] could not rename custom font {} to '{name}': {err}",
                    self.identity
                ));
                self.rename_error = Some(store_error_message(&err));
            }
        }
    }

    /// Draws the «Редактирование гибрида» body: the shared editor with its own «Применить»;
    /// a committed recipe replaces the stored one (identity unchanged). `catalog` is `None`
    /// while the font lists load.
    pub(super) fn draw_hybrid_section(&mut self, ui: &mut egui::Ui, catalog: Option<&SourceCatalog>) {
        let Some(editor) = self.editor.as_mut() else {
            ui.small(t!("typing.fonts.custom_font_invalid_recipe_tooltip"));
            return;
        };
        let Some(catalog) = catalog else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t!("typing.font_settings.loading_status"));
            });
            return;
        };
        match font_admin::draw_hybrid_editor(ui, editor, &HybridEditorCx { catalog, commit: CommitButton::Apply }) {
            HybridEditorOutcome::Idle | HybridEditorOutcome::CommitFailed => {}
            HybridEditorOutcome::Committed(committed) => {
                match font_admin::update_custom_font_recipe(&self.identity, (*committed.recipe).clone()) {
                    Ok(()) => self.apply_error = None,
                    Err(err) => {
                        runtime_log::log_error(format!(
                            "[settings] could not update the recipe of custom font {}: {err}; recipe={:?}",
                            self.identity, committed.recipe
                        ));
                        self.apply_error = Some(store_error_message(&err));
                    }
                }
            }
        }
        if let Some(error) = &self.apply_error {
            ui.colored_label(ui.visuals().error_fg_color, error.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_name_is_refused() {
        assert_eq!(validate_custom_font_name("", &[]), Err(CustomFontNameError::Blank));
        assert_eq!(validate_custom_font_name("   ", &["Гибрид"]), Err(CustomFontNameError::Blank));
    }

    #[test]
    fn a_name_is_trimmed_and_accepted_when_free() {
        assert_eq!(validate_custom_font_name("  Мой гибрид ", &["Другой", "Roboto"]), Ok("Мой гибрид".to_string()));
    }

    #[test]
    fn a_clash_with_a_custom_or_real_name_is_refused_case_insensitively() {
        // A custom font's name (Cyrillic case folding, not just ASCII).
        assert_eq!(validate_custom_font_name("ГИБРИД", &["гибрид"]), Err(CustomFontNameError::Taken));
        // A real font's display label, padded in the list.
        assert_eq!(validate_custom_font_name("roboto", &[" Roboto "]), Err(CustomFontNameError::Taken));
        // A near miss is not a clash.
        assert_eq!(validate_custom_font_name("Roboto 2", &["Roboto"]), Ok("Roboto 2".to_string()));
    }

    #[test]
    fn delete_needs_two_single_clicks() {
        // First click arms, second click on the armed button confirms.
        assert_eq!(delete_step(false, true, false, true), DeleteStep::Arm);
        assert_eq!(delete_step(true, true, false, true), DeleteStep::Confirm);
        // A physical double click never confirms: its second press is not a plain click.
        assert_eq!(delete_step(true, true, true, true), DeleteStep::Stay);
        assert_eq!(delete_step(false, true, true, true), DeleteStep::Stay);
    }

    #[test]
    fn an_armed_delete_disarms_when_the_pointer_leaves() {
        assert_eq!(delete_step(true, false, false, false), DeleteStep::Disarm);
        assert_eq!(delete_step(true, false, false, true), DeleteStep::Stay);
        // An unarmed button the pointer is not on does nothing.
        assert_eq!(delete_step(false, false, false, false), DeleteStep::Stay);
    }

    /// An unavailable row as the font lists produce it.
    fn unavailable(identity: &str, name: &str, reason: font_admin::CustomFontUnavailability) -> CustomFontRow {
        CustomFontRow { identity: identity.to_string(), name: name.to_string(), font: None, unavailable: Some(reason) }
    }

    #[test]
    fn rows_are_named_by_their_stored_name_with_the_identity_as_fallback() {
        let missing = unavailable(
            "MSCustom-0123456789abcdef",
            " Гибрид ",
            font_admin::CustomFontUnavailability::MissingSources(vec!["Roboto-Regular".to_string()]),
        );
        assert_eq!(custom_row_name(&missing), "Гибрид");
        let damaged = unavailable(
            "MSCustom-fedcba9876543210",
            "  ",
            font_admin::CustomFontUnavailability::InvalidRecipe("bad weight".to_string()),
        );
        assert_eq!(custom_row_name(&damaged), "MSCustom-fedcba9876543210");
    }

    #[test]
    fn every_store_refusal_has_a_message() {
        // Exhaustive by construction (no catch-all arm); this pins that a name clash reaching
        // the store reads like the field's own validation.
        assert_eq!(store_error_message(&CustomFontStoreError::NameTaken), CustomFontNameError::Taken.message());
        assert_eq!(store_error_message(&CustomFontStoreError::BlankName), CustomFontNameError::Blank.message());
        assert_eq!(
            store_error_message(&CustomFontStoreError::IdentityCollision),
            store_error_message(&CustomFontStoreError::Unsupported)
        );
    }
}
