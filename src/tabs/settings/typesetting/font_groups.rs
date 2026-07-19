/*
File: settings/typesetting/font_groups.rs

Purpose:
The "Группы" section of the settings "Настройки шрифтов" block: create, list, rename,
delete VIRTUAL font groups (user-defined named sets of real fonts) and edit each group's
members and per-group display aliases. UI ONLY — the group MODEL lives in
`crate::tabs::typing` and is reached exclusively through the `font_admin` facade.

Main responsibilities:
- render the create row (name field + validation against existing virtual groups AND real
  folder-group names) and the group list with an inline two-step delete confirm;
- own the floating group-editor window (`GroupEditorState`): rename, a virtualized member
  list with per-member alias editing/removal, and an inline add-member picker mirroring the
  system-font import picker body;
- cache the virtual-group snapshot and refresh it when `font_admin::fonts_revision` advances
  (every group mutation bumps the shared revision).

Key types:
- `FontGroupsEditorState` (owned by `FontSettingsEditorState`)
- `GroupEditorState` (the open editor window, at most one at a time)

Notes:
Folder-group names are HEAVY to enumerate (filesystem I/O), so they are loaded in the same
off-thread pass as the font categories (see `font_settings.rs`) and passed into `ui`; this
module never touches the filesystem on the GUI thread. Virtual-group reads/mutations are
in-memory (GUI-thread safe) through `crate::tabs::typing::font_admin`. The member list and the
add-member picker render each font name in its OWN typeface, reusing the shared
`crate::widgets::font_preview` registration helpers exactly like the import picker: only
VISIBLE rows register (the lists are virtualized), and a per-window family cap
(`PICKER_PREVIEW_FONT_CAP`) bounds egui's non-evicting font atlas — rows beyond the cap fall
back to the default font. Registering a font inherently needs its bytes, so per-visible-font
one-time file reads happen on the GUI thread (the heavy enumeration never does).
*/

use super::font_settings::{
    PICKER_PREVIEW_FONT_CAP, PREVIEW_ROW_HEIGHT_FACTOR, clean_font_display_name, font_row_matches,
};
use crate::tabs::typing::font_admin::{self, FontEntry, VirtualFontGroupInfo, VirtualFontGroupMemberInfo};
use crate::widgets::{combo_font_family_name, ensure_font_family, is_font_family_bound};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Maximum height (points) of the virtualized member list before it scrolls internally.
const MEMBER_LIST_MAX_HEIGHT: f32 = 240.0;
/// Minimum row height (points) of one member row (TextEdit + buttons headroom). The own-typeface
/// name can be taller, so the effective row height is the max of this and the preview height.
const MEMBER_ROW_HEIGHT: f32 = 30.0;
/// Maximum height (points) of the add-member picker result list before it scrolls.
const ADD_PICKER_MAX_HEIGHT: f32 = 240.0;
/// Width (points) of the per-member alias text field.
const ALIAS_EDIT_WIDTH: f32 = 160.0;

/// Member-name resolver: font file path → (cleaned display name, representative face index).
/// The face index feeds the member row's own-typeface preview registration.
type MemberResolver = HashMap<PathBuf, (String, usize)>;

/// Editor for the "Группы" section: create/list/delete virtual groups plus the group-editor
/// window. Caches the virtual-group snapshot and refreshes it when the shared font-config
/// revision advances. Owned by `FontSettingsEditorState`; talks only to `font_admin`.
#[derive(Default, Debug)]
pub(crate) struct FontGroupsEditorState {
    /// Cached virtual-group snapshot; refreshed when `groups_revision` goes stale.
    groups: Vec<VirtualFontGroupInfo>,
    /// Store revision at which `groups` was cached; `None` until the first refresh.
    groups_revision: Option<u64>,
    /// New-group name input buffer.
    new_group_name: String,
    /// Localized validation error shown under the create row, if any.
    create_error: Option<String>,
    /// Group currently ARMED for the two-step delete confirm (`None` = disarmed).
    delete_armed: Option<String>,
    /// The open group-editor window, if any (at most one at a time).
    editor: Option<GroupEditorState>,
}

impl FontGroupsEditorState {
    /// Renders the "Группы" collapsing section and the (independently floating) group-editor
    /// window. `folder_group_names` are the real folder-group names (loaded off-thread in the
    /// caller's font-category pass) used to reject name collisions on BOTH create and rename;
    /// `folder_fonts`/`imported_fonts` are the loaded categories used to resolve member display
    /// names and to populate the add-member picker. `categories_revision` is the snapshot
    /// revision of those loaded categories (`FontCategories::loaded_revision`), used to cache
    /// the editor window's path→display-name resolver and rebuild it only when the snapshot is
    /// replaced.
    ///
    /// `force_reveal` (set only on a deep-link reveal frame) force-opens the header and
    /// scrolls it to the top of the ancestor scroll area. Returns the section's block rect
    /// (header row unioned with the body when expanded), for the caller's reveal highlight.
    pub(crate) fn ui(
        &mut self,
        ui: &mut egui::Ui,
        folder_group_names: &[String],
        folder_fonts: &[FontEntry],
        imported_fonts: &[FontEntry],
        categories_revision: u64,
        force_reveal: bool,
    ) -> egui::Rect {
        self.refresh_cache();

        // `.open(None)` off the reveal frame leaves the persisted collapsed state alone, so
        // the user can collapse the section again after the deep link opened it.
        let header = egui::CollapsingHeader::new(t!("typing.font_settings.groups_header"))
            .id_salt("font_settings_groups")
            .open(force_reveal.then_some(true))
            .default_open(false)
            .show(ui, |ui| {
                self.draw_create_row(ui, folder_group_names);
                ui.add_space(6.0);
                self.draw_group_list(ui);
            });

        if force_reveal {
            // Bring the freshly-revealed groups block to the top of the settings scroll
            // area; the ancestor ScrollArea consumes this target on the next frame.
            header.header_response.scroll_to_me(Some(egui::Align::TOP));
        }

        // Full block rect: header row unioned with the body when expanded, for the highlight.
        let block_rect = match &header.body_response {
            Some(body) => header.header_response.rect.union(body.rect),
            None => header.header_response.rect,
        };

        // The editor window floats independently of the collapsing state, so it is drawn
        // OUTSIDE the header closure: collapsing the section must not close an open window.
        self.draw_group_editor_window(
            ui.ctx(),
            folder_group_names,
            folder_fonts,
            imported_fonts,
            categories_revision,
        );

        block_rect
    }

    /// Reloads the cached virtual-group snapshot when the shared font-config revision advances,
    /// and drops stale UI references (a pending delete arm or an editor window whose group
    /// vanished, e.g. deleted from another surface).
    fn refresh_cache(&mut self) {
        let current = font_admin::fonts_revision();
        if self.groups_revision == Some(current) {
            return;
        }
        self.groups = font_admin::list_virtual_groups();
        self.groups_revision = Some(current);
        if let Some(armed) = &self.delete_armed
            && !self.groups.iter().any(|group| &group.name == armed)
        {
            self.delete_armed = None;
        }
        if let Some(editor) = &self.editor
            && !self.groups.iter().any(|group| group.name == editor.group_name)
        {
            self.editor = None;
        }
    }

    /// Renders the create row (name field + "Создать") and any validation error.
    fn draw_create_row(&mut self, ui: &mut egui::Ui, folder_group_names: &[String]) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_group_name)
                    .id_salt("typing.font_settings.group_create_edit")
                    .desired_width(220.0)
                    .hint_text(t!("typing.font_settings.group_create_placeholder")),
            );
            if ui
                .button(t!("typing.font_settings.group_create_button"))
                .clicked()
            {
                self.try_create_group(folder_group_names);
            }
        });
        if let Some(err) = &self.create_error {
            let color = ui.visuals().error_fg_color;
            ui.colored_label(color, err.as_str());
        }
    }

    /// Validates and creates a new virtual group from the input buffer. Rejects a blank name
    /// or a case-insensitive collision with an existing virtual group OR a real folder-group
    /// name (the store cannot see the filesystem, so the folder-name check happens here). On
    /// success clears the field and the error; on rejection sets a localized error.
    fn try_create_group(&mut self, folder_group_names: &[String]) {
        self.delete_armed = None;
        let name = self.new_group_name.trim();
        if name.is_empty() {
            self.create_error =
                Some(t!("typing.font_settings.group_name_empty_error").to_string());
            return;
        }
        let lower = name.to_lowercase();
        let collides = self
            .groups
            .iter()
            .any(|group| group.name.to_lowercase() == lower)
            || folder_group_names
                .iter()
                .any(|folder| folder.to_lowercase() == lower);
        if collides {
            self.create_error =
                Some(t!("typing.font_settings.group_name_taken_error").to_string());
            return;
        }
        if font_admin::create_virtual_group(name) {
            self.new_group_name.clear();
            self.create_error = None;
        } else {
            // The store also rejects blanks/duplicates; surface a generic "taken" message.
            self.create_error =
                Some(t!("typing.font_settings.group_name_taken_error").to_string());
        }
    }

    /// Renders one row per virtual group: name + member count, an edit button opening the
    /// editor window, and the two-step delete control.
    fn draw_group_list(&mut self, ui: &mut egui::Ui) {
        if self.groups.is_empty() {
            ui.small(t!("typing.font_settings.groups_empty_hint"));
            return;
        }
        // Move the snapshot out so the row closures can mutate `self` (arm delete, open the
        // editor) without aliasing the borrowed list; restore afterward.
        let groups = std::mem::take(&mut self.groups);
        for group in &groups {
            ui.horizontal(|ui| {
                ui.label(tf!(
                    "typing.font_settings.group_row_label",
                    name = group.name,
                    count = group.members.len()
                ));
                if ui
                    .button(t!("typing.font_settings.group_edit_button"))
                    .clicked()
                {
                    self.open_editor(group);
                }
                self.draw_delete_control(ui, &group.name);
            });
        }
        self.groups = groups;
    }

    /// Draws the inline two-step delete control for one group. First click ARMS the group
    /// (button switches to "Удалить?"); a second click while armed deletes it. Clicking a
    /// different group's control re-arms to that group instead.
    ///
    /// Hardening: a physical DOUBLE-click cannot delete — the confirming click is gated on
    /// `!response.double_clicked()`, so the arm and confirm can never land in one gesture.
    /// The armed state also AUTO-DISARMS once the pointer leaves the armed button (it is not
    /// hovered this frame), so a stale arm cannot linger and turn a later unrelated click into
    /// an accidental delete.
    fn draw_delete_control(&mut self, ui: &mut egui::Ui, group_name: &str) {
        let armed = self.delete_armed.as_deref() == Some(group_name);
        let button = if armed {
            // Armed state is tinted red so the destructive confirm reads clearly.
            egui::Button::new(t!("typing.font_settings.group_delete_confirm_button"))
                .fill(egui::Color32::from_rgb(150, 40, 40))
        } else {
            egui::Button::new(t!("typing.font_settings.group_delete_button"))
        };
        let response = ui.add(button);
        // A double-click delivers a press on two consecutive frames; without this guard the
        // first press arms and the second confirms, deleting on a single physical gesture.
        // Requiring a plain single click for the confirm step forces two deliberate clicks.
        if response.clicked() && !response.double_clicked() {
            if armed {
                font_admin::delete_virtual_group(group_name);
                self.delete_armed = None;
            } else {
                self.delete_armed = Some(group_name.to_string());
            }
        } else if armed && !response.hovered() {
            // Pointer moved away from the armed button: disarm so a later click elsewhere
            // cannot be mistaken for the confirm step.
            self.delete_armed = None;
        }
    }

    /// Opens the editor window for `group`, seeding the rename buffer and the per-member alias
    /// buffers from the current snapshot. Replaces any currently-open editor.
    fn open_editor(&mut self, group: &VirtualFontGroupInfo) {
        self.delete_armed = None;
        let alias_bufs = group
            .members
            .iter()
            .map(|member| (member.path.clone(), member.alias.clone().unwrap_or_default()))
            .collect();
        self.editor = Some(GroupEditorState {
            group_name: group.name.clone(),
            rename_buf: group.name.clone(),
            rename_error: None,
            alias_bufs,
            add_open: false,
            add_search: String::new(),
            add_selected: None,
            resolver_cache: None,
            preview_families: HashSet::new(),
        });
    }

    /// Renders the group-editor window when open; drops its state once the user closes it or
    /// the edited group disappears. Resolves member display names from the loaded categories,
    /// caching the resolver per `categories_revision`. `folder_group_names` are threaded down so
    /// the rename step can reject a collision with a real folder group.
    fn draw_group_editor_window(
        &mut self,
        ctx: &egui::Context,
        folder_group_names: &[String],
        folder_fonts: &[FontEntry],
        imported_fonts: &[FontEntry],
        categories_revision: u64,
    ) {
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        // Current members come from the revision-refreshed snapshot; clone so the window
        // closure does not alias `self`.
        let members = self
            .groups
            .iter()
            .find(|group| group.name == editor.group_name)
            .map(|group| group.members.clone())
            .unwrap_or_default();

        // path -> (resolved display name, representative face index). The face index is kept so
        // the member list can register the font in its OWN typeface (own-typeface preview) without
        // re-reaching the FontEntry per frame. Rebuilding this over folder+imported fonts every
        // frame (plus a String per font) is wasteful while the window stays open, so it is
        // cached and rebuilt only when the categories snapshot is replaced (revision advance).
        // The map is moved out of `editor` so the window body can borrow `editor` mutably
        // without aliasing it, then restored below.
        let needs_rebuild = editor
            .resolver_cache
            .as_ref()
            .is_none_or(|(rev, _)| *rev != categories_revision);
        let resolver: MemberResolver = if needs_rebuild {
            let mut map: MemberResolver = HashMap::new();
            for font in folder_fonts.iter().chain(imported_fonts.iter()) {
                map.entry(font.path().to_path_buf()).or_insert_with(|| {
                    (
                        clean_font_display_name(font.display_label()),
                        font.representative_face_index(),
                    )
                });
            }
            map
        } else {
            // Cache is fresh: reuse it (moved out, restored after the window closure).
            editor
                .resolver_cache
                .take()
                .map(|(_, map)| map)
                .unwrap_or_default()
        };

        let title = tf!(
            "typing.font_settings.group_editor_window_title",
            name = editor.group_name
        );
        let mut window_open = true;
        egui::Window::new(title)
            // The title carries the group name, so pin a stable id (05-ids-and-i18n.md).
            .id(egui::Id::new("typing.font_settings.group_editor_window"))
            .open(&mut window_open)
            .collapsible(false)
            .resizable(true)
            .default_size([460.0, 520.0])
            // Inner sections carry their own bounded scroll areas; the window must not add a
            // second vscroll on top of them.
            .vscroll(false)
            .show(ctx, |ui| {
                editor.draw_body(
                    ui,
                    &members,
                    &resolver,
                    folder_group_names,
                    folder_fonts,
                    imported_fonts,
                );
            });

        // Restore the (possibly rebuilt) resolver into the cache for the next frame.
        editor.resolver_cache = Some((categories_revision, resolver));

        if window_open {
            self.editor = Some(editor);
        }
    }
}

/// State of the open group-editor window (at most one at a time). Owned by
/// `FontGroupsEditorState`; dropped when the window closes.
#[derive(Default, Debug)]
struct GroupEditorState {
    /// The group being edited. Updated in place on a successful rename.
    group_name: String,
    /// Rename input buffer (seeded from `group_name` at open).
    rename_buf: String,
    /// Localized validation error shown under the rename row, if any. Mirrors the create
    /// row's `create_error`: set on a rejected rename, cleared on success or a text edit.
    rename_error: Option<String>,
    /// Per-member alias edit buffers, keyed by member font path.
    alias_bufs: HashMap<PathBuf, String>,
    /// Whether the inline add-member picker is expanded.
    add_open: bool,
    /// Case-insensitive search filter for the add-member picker.
    add_search: String,
    /// Selected candidate font path in the add-member picker.
    add_selected: Option<PathBuf>,
    /// Cached path→(display name, representative face index) resolver for the member list, keyed
    /// by the categories snapshot revision it was built from. Rebuilt only when that revision
    /// advances. The face index feeds the member row's own-typeface preview.
    resolver_cache: Option<(u64, MemberResolver)>,
    /// egui family names this window has previewed in their own typeface (member list AND add
    /// picker share one set). Bounds one-time non-evicting `add_font` growth via
    /// `PICKER_PREVIEW_FONT_CAP`; persists while the window stays open and resets on reopen.
    preview_families: HashSet<String>,
}

impl GroupEditorState {
    /// Renders the whole window body: rename row, member list, and the add-member section.
    fn draw_body(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        resolver: &MemberResolver,
        folder_group_names: &[String],
        folder_fonts: &[FontEntry],
        imported_fonts: &[FontEntry],
    ) {
        self.draw_rename_row(ui, folder_group_names);
        ui.add_space(6.0);
        ui.separator();

        self.draw_members(ui, members, resolver);
        ui.add_space(6.0);
        ui.separator();

        self.draw_add_section(ui, members, folder_fonts, imported_fonts);
    }

    /// Renders the rename row: a text field prefilled with the current name plus an apply
    /// button, and a red validation error below it when a rename was rejected. Applying also
    /// happens on Enter while the field has focus; editing the text clears a stale error.
    fn draw_rename_row(&mut self, ui: &mut egui::Ui, folder_group_names: &[String]) {
        ui.label(t!("typing.font_settings.group_rename_label"));
        let (response, apply_clicked) = ui
            .horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.rename_buf)
                        .id_salt("typing.font_settings.group_rename_edit")
                        .desired_width(260.0)
                        .hint_text(self.group_name.as_str()),
                );
                let apply_clicked = ui
                    .button(t!("typing.font_settings.properties_apply_button"))
                    .clicked();
                (response, apply_clicked)
            })
            .inner;
        // Editing the field clears a previously shown error so it does not linger over text
        // the user is actively correcting (mirrors the create row's error lifecycle).
        if response.changed() {
            self.rename_error = None;
        }
        let submitted =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if submitted || apply_clicked {
            self.apply_rename(folder_group_names);
        }
        if let Some(err) = &self.rename_error {
            let color = ui.visuals().error_fg_color;
            ui.colored_label(color, err.as_str());
        }
    }

    /// Applies the rename buffer as the new group name. A blank name and any collision are
    /// rejected with a localized error surfaced under the row; an unchanged name is a silent
    /// no-op. The collision check mirrors `try_create_group`: the store's `rename_virtual_group`
    /// only sees VIRTUAL groups, so a case-insensitive clash with a real FOLDER group is
    /// rejected here (otherwise the panel's `apply_virtual_groups` would silently drop the
    /// renamed group). On a successful store rename the window follows the new name and the
    /// error is cleared; on rejection the old name is kept and the buffer retains the user's
    /// text so they can correct it.
    fn apply_rename(&mut self, folder_group_names: &[String]) {
        let new_name = self.rename_buf.trim().to_string();
        if new_name.is_empty() {
            self.rename_error =
                Some(t!("typing.font_settings.group_name_empty_error").to_string());
            return;
        }
        if new_name == self.group_name {
            // Unchanged name: nothing to do, and no error to show.
            self.rename_error = None;
            return;
        }
        let lower = new_name.to_lowercase();
        if folder_group_names
            .iter()
            .any(|folder| folder.to_lowercase() == lower)
        {
            self.rename_error =
                Some(t!("typing.font_settings.group_name_taken_error").to_string());
            return;
        }
        if font_admin::rename_virtual_group(&self.group_name, &new_name) {
            self.group_name = new_name;
            self.rename_error = None;
        } else {
            // The store rejects a case-insensitive clash with another VIRTUAL group (blank and
            // unchanged names are already handled above), so surface it as "name taken".
            self.rename_error =
                Some(t!("typing.font_settings.group_name_taken_error").to_string());
        }
    }

    /// Renders the virtualized member list: per row the resolved display name rendered in the
    /// font's OWN typeface (or a greyed, plain "файл не найден" note when no loaded font
    /// matches), an alias field + apply, and a remove button. Own-typeface registration is
    /// bounded by the shared `preview_families` cap and only runs for VISIBLE rows. Store
    /// mutations are collected and applied after the scroll closure so no mutation happens
    /// mid-iteration.
    fn draw_members(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        resolver: &MemberResolver,
    ) {
        ui.label(tf!(
            "typing.font_settings.group_members_header",
            count = members.len()
        ));
        if members.is_empty() {
            ui.small(t!("typing.font_settings.group_no_members_hint"));
            return;
        }

        let group_name = self.group_name.clone();
        // Deferred store actions (applied after the scroll closure).
        let mut alias_to_apply: Option<(PathBuf, String)> = None;
        let mut member_to_remove: Option<PathBuf> = None;

        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        // Own-typeface names can be taller than the default body; keep the alias field's
        // headroom as a floor so short-lined fonts still lay out cleanly.
        let row_height = MEMBER_ROW_HEIGHT.max(body_size * PREVIEW_ROW_HEIGHT_FACTOR);
        egui::ScrollArea::vertical()
            .id_salt("typing.font_settings.group_members_list")
            .max_height(MEMBER_LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, members.len(), |ui, range| {
                for row in range {
                    let Some(member) = members.get(row) else {
                        continue;
                    };
                    // Lazily seed a buffer for members added after the window opened.
                    let buf = self
                        .alias_bufs
                        .entry(member.path.clone())
                        .or_insert_with(|| member.alias.clone().unwrap_or_default());
                    ui.horizontal(|ui| {
                        match resolver.get(member.path.as_path()) {
                            Some((name, face)) => {
                                // Render the display name in the member font's own typeface,
                                // registered on first VISIBLE use and bounded by the shared cap.
                                let prev_override = ui.style().override_font_id.clone();
                                if let Some(font_id) = own_typeface_font_id(
                                    ui.ctx(),
                                    member.path.as_path(),
                                    *face,
                                    body_size,
                                    &mut self.preview_families,
                                ) {
                                    ui.style_mut().override_font_id = Some(font_id);
                                }
                                ui.label(name.as_str());
                                ui.style_mut().override_font_id = prev_override;
                            }
                            None => {
                                // Keep the entry; just flag that its file is not currently
                                // loaded (do NOT auto-remove it).
                                let file_name = member
                                    .path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| {
                                        member.path.to_string_lossy().into_owned()
                                    });
                                ui.add(egui::Label::new(egui::RichText::new(file_name).weak()))
                                    .on_hover_text(t!(
                                        "typing.font_settings.group_member_missing_hint"
                                    ));
                            }
                        }
                        let response = ui.add(
                            egui::TextEdit::singleline(buf)
                                .id_salt((
                                    "typing.font_settings.group_member_alias_edit",
                                    member.path.as_path(),
                                ))
                                .desired_width(ALIAS_EDIT_WIDTH)
                                .hint_text(t!(
                                    "typing.font_settings.group_member_alias_placeholder"
                                )),
                        );
                        let submitted = response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        if ui
                            .button(t!("typing.font_settings.properties_apply_button"))
                            .clicked()
                            || submitted
                        {
                            alias_to_apply = Some((member.path.clone(), buf.clone()));
                        }
                        if ui
                            .small_button("✕")
                            .on_hover_text(t!(
                                "typing.font_settings.group_member_remove_tooltip"
                            ))
                            .clicked()
                        {
                            member_to_remove = Some(member.path.clone());
                        }
                    });
                }
            });

        if let Some((path, alias)) = alias_to_apply {
            let trimmed = alias.trim();
            // Blank clears the alias (reset to the font's own label).
            let value = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            font_admin::set_virtual_group_member_alias(&group_name, &path, value);
        }
        if let Some(path) = member_to_remove {
            font_admin::remove_virtual_group_member(&group_name, &path);
            self.alias_bufs.remove(&path);
        }
    }

    /// Renders the add-member section: a "Добавить шрифт" button that expands into a picker
    /// (search + virtualized candidate rows + confirm/cancel) over the folder and imported
    /// fonts NOT already members. Each candidate row is drawn in its OWN typeface (registered
    /// on first VISIBLE use, bounded by the shared `preview_families` cap), mirroring the
    /// system-import picker.
    fn draw_add_section(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        folder_fonts: &[FontEntry],
        imported_fonts: &[FontEntry],
    ) {
        if !self.add_open {
            if ui
                .button(t!("typing.font_settings.group_add_font_button"))
                .clicked()
            {
                self.add_open = true;
                self.add_search.clear();
                self.add_selected = None;
            }
            return;
        }

        // Candidates = folder + imported fonts that are not already members.
        let member_paths: HashSet<&Path> =
            members.iter().map(|member| member.path.as_path()).collect();
        let candidates: Vec<&FontEntry> = folder_fonts
            .iter()
            .chain(imported_fonts.iter())
            .filter(|font| !member_paths.contains(font.path()))
            .collect();

        ui.label(t!("typing.font_settings.group_add_font_header"));
        ui.horizontal(|ui| {
            ui.label(t!("typing.font_settings.search_label"));
            ui.add(
                egui::TextEdit::singleline(&mut self.add_search)
                    .id_salt("typing.font_settings.group_add_search_edit")
                    .desired_width(240.0)
                    .hint_text(t!("typing.font_settings.search_placeholder")),
            );
        });

        // Filter once; only indices survive so the virtualized list can index back.
        let filtered: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, font)| {
                font_row_matches(
                    font.label(),
                    font.original_name(),
                    font.display_label(),
                    &self.add_search,
                )
            })
            .map(|(idx, _)| idx)
            .collect();

        if filtered.is_empty() {
            ui.small(t!("typing.font_settings.nothing_found_status"));
        } else {
            let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
            // Own-typeface rows can exceed `body_size`; give the same headroom as the import
            // picker so `show_rows` positions rows without clipping.
            let row_height = body_size * PREVIEW_ROW_HEIGHT_FACTOR;
            egui::ScrollArea::vertical()
                .id_salt("typing.font_settings.group_add_list")
                .max_height(ADD_PICKER_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show_rows(ui, row_height, filtered.len(), |ui, range| {
                    for row in range {
                        let Some(&idx) = filtered.get(row) else {
                            continue;
                        };
                        let font = candidates[idx];
                        let is_selected = self.add_selected.as_deref() == Some(font.path());
                        // Preview the candidate in its own typeface, bounded by the shared cap.
                        let prev_override = ui.style().override_font_id.clone();
                        if let Some(font_id) = own_typeface_font_id(
                            ui.ctx(),
                            font.path(),
                            font.representative_face_index(),
                            body_size,
                            &mut self.preview_families,
                        ) {
                            ui.style_mut().override_font_id = Some(font_id);
                        }
                        let clicked = ui
                            .selectable_label(
                                is_selected,
                                clean_font_display_name(font.display_label()),
                            )
                            .clicked();
                        ui.style_mut().override_font_id = prev_override;
                        if clicked {
                            self.add_selected = Some(font.path().to_path_buf());
                        }
                    }
                });
        }

        ui.separator();
        let group_name = self.group_name.clone();
        ui.horizontal(|ui| {
            let can_add = self.add_selected.is_some();
            if ui
                .add_enabled(
                    can_add,
                    egui::Button::new(t!("typing.font_settings.add_button")),
                )
                .clicked()
            {
                if let Some(path) = self.add_selected.clone() {
                    font_admin::add_virtual_group_member(&group_name, &path);
                }
                self.close_add_section();
            }
            if ui.button(t!("typing.common.cancel_button")).clicked() {
                self.close_add_section();
            }
        });
    }

    /// Collapses the add-member picker and resets its transient search/selection state.
    fn close_add_section(&mut self) {
        self.add_open = false;
        self.add_selected = None;
        self.add_search.clear();
    }
}

/// Returns the `FontId` to override the style with to preview the font at `(path, face)` in its
/// OWN typeface, or `None` to keep the default font.
///
/// Mirrors the import picker's registration discipline (`font_settings::draw_picker_body`):
/// the family previews only if it is already bound, already previewed this window session, or
/// still under `PICKER_PREVIEW_FONT_CAP` — egui's `add_font` never evicts, so an unbounded
/// scroll would otherwise leak font atlases. On first eligible use it registers the font
/// (reading its bytes on the GUI thread) and records the family in `preview_families`. Returns
/// `None` (default font) beyond the cap or when the file cannot be registered; never panics.
fn own_typeface_font_id(
    ctx: &egui::Context,
    path: &Path,
    face: usize,
    body_size: f32,
    preview_families: &mut HashSet<String>,
) -> Option<egui::FontId> {
    let font_name = combo_font_family_name(path, face);
    let allow_own = is_font_family_bound(ctx, &egui::FontFamily::Name(font_name.clone().into()))
        || preview_families.contains(&font_name)
        || preview_families.len() < PICKER_PREVIEW_FONT_CAP;
    if !allow_own {
        return None;
    }
    let family = ensure_font_family(ctx, path, face)?;
    preview_families.insert(font_name);
    Some(egui::FontId::new(body_size, family))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A group editor seeded on `name`, with the rename buffer prefilled to it.
    fn editor(name: &str) -> GroupEditorState {
        GroupEditorState {
            group_name: name.to_string(),
            rename_buf: name.to_string(),
            ..GroupEditorState::default()
        }
    }

    #[test]
    fn apply_rename_rejects_blank_name() {
        let mut ed = editor("Экшн");
        ed.rename_buf = "   ".to_string();
        ed.apply_rename(&[]);
        assert_eq!(
            ed.rename_error,
            Some(t!("typing.font_settings.group_name_empty_error").to_string()),
            "a blank name is rejected with the empty-name error"
        );
        assert_eq!(ed.group_name, "Экшн", "a rejected rename keeps the old name");
    }

    #[test]
    fn apply_rename_rejects_folder_group_collision_case_insensitively() {
        let mut ed = editor("Экшн");
        ed.rename_buf = "manga".to_string();
        // A real folder group "Manga" exists; the store cannot see it, so the UI must reject
        // the rename here (otherwise the panel's merge would silently drop the renamed group).
        ed.apply_rename(&["Manga".to_string()]);
        assert_eq!(
            ed.rename_error,
            Some(t!("typing.font_settings.group_name_taken_error").to_string()),
            "a case-insensitive clash with a real folder group is rejected"
        );
        assert_eq!(ed.group_name, "Экшн");
    }

    #[test]
    fn apply_rename_unchanged_name_clears_stale_error() {
        let mut ed = editor("Экшн");
        ed.rename_error = Some("stale".to_string());
        // The trimmed buffer equals the current name → silent no-op, and any prior error clears.
        ed.apply_rename(&["Manga".to_string()]);
        assert_eq!(ed.rename_error, None);
        assert_eq!(ed.group_name, "Экшн");
    }
}
