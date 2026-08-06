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
- own the floating group-editor window (`GroupEditorState`): a rename field, a virtualized
  member TABLE (own-typeface name / identity / per-group alias / remove), an inline
  add-member picker mirroring the system-font import picker body, and ONE "Применить" button
  that commits the rename and every changed alias together;
- offer ONE name-display switch per editor window, now scoped to the ADD-MEMBER picker only:
  the member table shows the user-facing name AND the identity side by side, so there is
  nothing left for the switch to choose there. It selects whether a candidate row shows the
  font's user-facing name or its identity (PostScript name). The widget, the modes and the
  name selection all come from `font_settings.rs`; this module only borrows the mode slot and
  draws rows with it;
- cache the virtual-group snapshot and refresh it when `font_admin::fonts_revision` advances
  (every group mutation bumps the shared revision).

Key types:
- `FontGroupsEditorState` (owned by `FontSettingsEditorState`)
- `GroupEditorFonts` (the loaded font data the section draws from, bundled for the call chain)
- `GroupEditorState` (the open editor window, at most one at a time)
- `MemberColumns` (the member table's column widths, shared by its header and its rows)

Notes:
EDIT MODEL: the rename field and the per-member alias fields are BUFFERS. Nothing they hold
reaches the store until the window's single "Применить" button (or Enter in one of those
fields) commits it; the button is disabled while no buffer differs from the store, so the
presence of unsaved edits is visible. Removing a member is the one exception — it is a
membership operation, not a text edit, and stays immediate. Closing the window DISCARDS every
uncommitted buffer with the editor state.

Folder-group names are HEAVY to enumerate (filesystem I/O), so they are loaded in the same
off-thread pass as the font categories (see `font_settings.rs`) and passed into `ui`; this
module never touches the filesystem on the GUI thread. Virtual-group reads/mutations are
in-memory (GUI-thread safe) through `crate::tabs::typing::font_admin`. The member table and the
add-member picker render each font name in its OWN typeface, reusing the shared
`crate::widgets::font_preview` registration helpers exactly like the import picker: only
VISIBLE rows register (the lists are virtualized), and a per-window family cap
(`PICKER_PREVIEW_FONT_CAP`) bounds egui's non-evicting font atlas — rows beyond the cap fall
back to the default font. The font FILES are read on `widgets::font_preview`'s own worker
threads, never here — a row draws in the interface font until its bytes are registered.
*/

use super::font_settings::{
    FontListKind, FontNameDisplayMode, PICKER_PREVIEW_FONT_CAP, PREVIEW_ROW_HEIGHT_FACTOR,
    draw_name_mode_switch, font_row_matches, font_row_name_for_mode, unavailable_row_name,
};
use crate::tabs::typing::font_admin::{self, FontEntry, VirtualFontGroupInfo, VirtualFontGroupMemberInfo};
use crate::widgets::{
    PreviewFontFamily, combo_font_family_name, is_font_family_bound, request_font_family,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Maximum height (points) of the virtualized member list before it scrolls internally.
const MEMBER_LIST_MAX_HEIGHT: f32 = 240.0;
/// Minimum row height (points) of one member row (TextEdit + buttons headroom). The own-typeface
/// name can be taller, so the effective row height is the max of this and the preview height.
const MEMBER_ROW_HEIGHT: f32 = 30.0;
/// Maximum height (points) of the add-member picker result list before it scrolls.
const ADD_PICKER_MAX_HEIGHT: f32 = 240.0;
/// Width (points) of the per-member alias text field — the member table's third column.
const ALIAS_EDIT_WIDTH: f32 = 160.0;
/// Width (points) of the member table's remove-button column.
const REMOVE_COL_WIDTH: f32 = 24.0;
/// Smallest width (points) either NAME column of the member table may shrink to. Below this a
/// truncated name carries no information at all, so the table is allowed to overflow the
/// window instead (the window's own `min_width` keeps that from happening in practice).
const MIN_NAME_COL_WIDTH: f32 = 90.0;
/// Columns of the member table: font name, identity, per-group alias, remove button.
const MEMBER_TABLE_COLUMNS: usize = 4;
/// Minimum window width (points) that keeps the whole member table inside the window:
/// both name columns at their floor, the two fixed columns, the three inter-column gaps at
/// the default item spacing, plus the window frame margins.
const EDITOR_WINDOW_MIN_WIDTH: f32 = 460.0;

/// Member-name resolver: font IDENTITY → the data a member row needs to draw itself.
///
/// The KEY is the stored member identity (`FontEntry::render_identity_name`), the same value
/// `fonts_data.json` records, so a member keeps resolving after its file is moved or
/// renamed. A key that resolves to nothing is a font that is not currently loaded (or a
/// stale legacy reference an unmigrated document still carries); its row is shown greyed and
/// is never auto-removed.
type MemberResolver = HashMap<String, MemberRowFont>;

/// What a group-member row needs about its font: its raw display label, the font's
/// CONTENT HASH and representative face index (together with the identity KEY, the
/// own-typeface preview registration key) and its file PATH, which is only the byte source
/// of that registration.
#[derive(Debug, Clone)]
struct MemberRowFont {
    /// `FontEntry::display_label` VERBATIM (system marker not yet stripped): the row's name is
    /// selected from it and the identity by `font_row_name_for_mode`, which does the cleaning,
    /// so this module never second-guesses the presentation rules.
    display_label: String,
    /// `FontEntry::content_hash` — the byte discriminant of the preview registration, so a
    /// replaced font file is not drawn from the bytes egui cached for the old one.
    content_hash: u64,
    rep_face: usize,
    /// Where the font's bytes live; passed to `widgets::request_font_family` as the byte
    /// source of the one-time registration and never used as a key.
    path: std::path::PathBuf,
}

/// Geometry of the member table, computed ONCE per frame and reused by the header row and by
/// every body row.
///
/// The four columns are laid out with EXPLICIT widths rather than left to `egui::Grid`'s
/// content-driven sizing, for two reasons that both bite here:
/// - the first column is painted in each font's OWN typeface, so its natural width varies
///   wildly between rows and the columns would visibly stagger;
/// - the list is VIRTUALIZED, so a content-sized column would resize as different rows scroll
///   into view (`Grid` derives a column's width from the cells it actually drew).
///
/// With fixed widths the columns are identical for every row, and the header — which lives
/// OUTSIDE the scroll area and therefore cannot be a grid row — lines up with them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MemberColumns {
    /// Width of the "Имя шрифта" column (own-typeface user-facing name).
    name: f32,
    /// Width of the "Название шрифта" column (the identity / PostScript name).
    identity: f32,
    /// Width of the "Имя в группе" column (the alias field).
    alias: f32,
    /// Width of the remove-button column.
    remove: f32,
    /// Height of one row; also the `show_rows` row height, so the grid rows and the
    /// virtualizer's row pitch agree.
    row_height: f32,
}

impl MemberColumns {
    /// Computes the table geometry for the width currently available to it.
    ///
    /// `available_width` is the width the table body may occupy (the caller subtracts the
    /// scroll-bar allowance so the header, drawn outside the scroll area, uses the same
    /// numbers as the rows inside it). `row_height` is the row pitch shared with
    /// `ScrollArea::show_rows`.
    fn new(available_width: f32, spacing_x: f32, row_height: f32) -> Self {
        let name = name_column_width(available_width, spacing_x);
        Self {
            name,
            identity: name,
            alias: ALIAS_EDIT_WIDTH,
            remove: REMOVE_COL_WIDTH,
            row_height,
        }
    }
}

/// Width one NAME column of the member table gets for `available_width`.
///
/// The alias field and the remove button keep fixed widths; whatever is left after them and
/// the three inter-column gaps (`spacing_x` each) is split evenly between the two name
/// columns, so widening the window widens the names. The result never drops below
/// [`MIN_NAME_COL_WIDTH`], in which case the table simply overflows the window.
fn name_column_width(available_width: f32, spacing_x: f32) -> f32 {
    let fixed = ALIAS_EDIT_WIDTH + REMOVE_COL_WIDTH + 3.0 * spacing_x;
    ((available_width - fixed) / 2.0).max(MIN_NAME_COL_WIDTH)
}

/// Runs `add_contents` inside one fixed-size table cell, its content left-aligned and
/// vertically centred, and returns whatever it produced.
///
/// The cell claims its full `width` even when the content is narrower ([`egui::Ui::set_min_width`]):
/// without that, `egui::Grid` would size the column to the widest cell it happened to draw and
/// the columns would jitter as the virtualized list scrolls. Inside a grid, `Ui::add_space` is
/// debug-asserted against, so a cell must never pad itself — request the width instead.
fn table_cell<R>(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            add_contents(ui)
        },
    )
    .inner
}

/// The loaded font data the groups section draws from, taken from the caller's off-thread
/// category snapshot (`font_settings::FontCategories`). Bundled into one borrow so the
/// section → window → body call chain stays readable instead of threading four parallel
/// parameters; it owns nothing and lives for one frame.
pub(super) struct GroupEditorFonts<'a> {
    /// Real folder-group names under `fonts/groups/`, used to reject a create/rename that
    /// collides with one (the store cannot see the filesystem).
    pub(super) folder_group_names: &'a [String],
    /// Fonts discovered in the `fonts/` folder.
    pub(super) folder: &'a [FontEntry],
    /// The loadable imported system fonts.
    pub(super) imported: &'a [FontEntry],
    /// Revision of the snapshot these lists came from; the editor window's member resolver is
    /// cached against it and rebuilt only when it advances.
    pub(super) categories_revision: u64,
}

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
    /// window. `fonts` carries the caller's off-thread category snapshot (folder-group names
    /// for the create/rename collision checks, the loaded font lists that resolve member names
    /// and fill the add-member picker, and the snapshot revision the member resolver is cached
    /// against).
    ///
    /// `name_mode` is the group-editor window's name-display mode, BORROWED from the owning
    /// widget's `FontNameDisplayModes`: the window's switch writes the user's choice straight
    /// into it, and the owner detects the change and persists it (this module owns no
    /// preference state and performs no I/O).
    ///
    /// `force_reveal` (set only on a deep-link reveal frame) force-opens the header and
    /// scrolls it to the top of the ancestor scroll area. Returns the section's block rect
    /// (header row unioned with the body when expanded), for the caller's reveal highlight.
    pub(crate) fn ui(
        &mut self,
        ui: &mut egui::Ui,
        fonts: &GroupEditorFonts<'_>,
        force_reveal: bool,
        name_mode: &mut FontNameDisplayMode,
    ) -> egui::Rect {
        self.refresh_cache();

        // `.open(None)` off the reveal frame leaves the persisted collapsed state alone, so
        // the user can collapse the section again after the deep link opened it.
        let header = egui::CollapsingHeader::new(t!("typing.font_settings.groups_header"))
            .id_salt("font_settings_groups")
            .open(force_reveal.then_some(true))
            .default_open(false)
            .show(ui, |ui| {
                self.draw_create_row(ui, fonts.folder_group_names);
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
        self.draw_group_editor_window(ui.ctx(), fonts, name_mode);

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
            .map(|member| (member.font.clone(), member.alias.clone().unwrap_or_default()))
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
    /// caching the resolver per `fonts.categories_revision`. `name_mode` is the window's
    /// name-display mode, borrowed from the owning widget (see [`Self::ui`]).
    fn draw_group_editor_window(
        &mut self,
        ctx: &egui::Context,
        fonts: &GroupEditorFonts<'_>,
        name_mode: &mut FontNameDisplayMode,
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

        // identity -> (resolved display name, representative face index). The face index is kept so
        // the member list can register the font in its OWN typeface (own-typeface preview) without
        // re-reaching the FontEntry per frame. Rebuilding this over folder+imported fonts every
        // frame (plus a String per font) is wasteful while the window stays open, so it is
        // cached and rebuilt only when the categories snapshot is replaced (revision advance).
        // The map is moved out of `editor` so the window body can borrow `editor` mutably
        // without aliasing it, then restored below.
        let needs_rebuild = editor
            .resolver_cache
            .as_ref()
            .is_none_or(|(rev, _)| *rev != fonts.categories_revision);
        let resolver: MemberResolver = if needs_rebuild {
            let mut map: MemberResolver = HashMap::new();
            for font in fonts.folder.iter().chain(fonts.imported.iter()) {
                map.entry(font.render_identity_name())
                    .or_insert_with(|| MemberRowFont {
                        display_label: font.display_label().to_string(),
                        content_hash: font.content_hash(),
                        rep_face: font.representative_face_index(),
                        path: font.path().to_path_buf(),
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
            .default_size([620.0, 560.0])
            // The member table has a minimum width of its own (two name columns at their
            // floor plus the two fixed columns); stop the user from resizing the window
            // narrower than that, which would clip the remove buttons out of reach.
            .min_width(EDITOR_WINDOW_MIN_WIDTH)
            // Inner sections carry their own bounded scroll areas; the window must not add a
            // second vscroll on top of them.
            .vscroll(false)
            .show(ctx, |ui| {
                editor.draw_body(ui, &members, &resolver, fonts, name_mode);
            });

        // Restore the (possibly rebuilt) resolver into the cache for the next frame.
        editor.resolver_cache = Some((fonts.categories_revision, resolver));

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
    /// Per-member alias edit buffers, keyed by member font IDENTITY.
    alias_bufs: HashMap<String, String>,
    /// Whether the inline add-member picker is expanded.
    add_open: bool,
    /// Case-insensitive search filter for the add-member picker.
    add_search: String,
    /// IDENTITY of the selected candidate font in the add-member picker.
    add_selected: Option<String>,
    /// Cached identity→[`MemberRowFont`] resolver for the member list, keyed by the categories
    /// snapshot revision it was built from. Rebuilt only when that revision advances. It holds
    /// the font's raw display label (the name-mode selector picks between it and the identity)
    /// plus the face index and content hash the row's own-typeface preview needs.
    resolver_cache: Option<(u64, MemberResolver)>,
    /// egui family names this window has previewed in their own typeface (member list AND add
    /// picker share one set). Bounds one-time non-evicting `add_font` growth via
    /// `PICKER_PREVIEW_FONT_CAP`; persists while the window stays open and resets on reopen.
    preview_families: HashSet<String>,
}

impl GroupEditorState {
    /// Renders the whole window body: rename field, the member table, the add-member section
    /// and the single "Применить" button that commits every buffered edit at once.
    ///
    /// The name-display switch is NOT drawn here any more: the member table shows the
    /// user-facing name and the identity side by side, so the only list left with a choice to
    /// make is the add-member picker, which now carries the switch itself.
    ///
    /// Enter in the rename field or in an alias field commits exactly what the button commits,
    /// so the two ways of confirming an edit cannot diverge.
    fn draw_body(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        resolver: &MemberResolver,
        fonts: &GroupEditorFonts<'_>,
        name_mode: &mut FontNameDisplayMode,
    ) {
        let rename_submitted = self.draw_rename_row(ui);
        ui.add_space(6.0);
        ui.separator();

        let alias_submitted = self.draw_members(ui, members, resolver);
        ui.add_space(6.0);
        ui.separator();

        self.draw_add_section(ui, members, fonts, name_mode);

        ui.add_space(6.0);
        ui.separator();
        let apply_clicked = self.draw_apply_row(ui, members);

        if apply_clicked || rename_submitted || alias_submitted {
            self.apply_changes(members, fonts.folder_group_names);
        }
    }

    /// Renders the rename row: a text field prefilled with the current name, and a red
    /// validation error below it when the last commit rejected the name. Returns whether the
    /// user pressed Enter in the field, which the caller treats exactly like the window's
    /// "Применить" button. Editing the text clears a stale error.
    ///
    /// The row carries NO button of its own: the rename is committed together with the alias
    /// edits by the single button at the bottom of the window.
    fn draw_rename_row(&mut self, ui: &mut egui::Ui) -> bool {
        ui.label(t!("typing.font_settings.group_rename_label"));
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.rename_buf)
                .id_salt("typing.font_settings.group_rename_edit")
                .desired_width(260.0)
                .hint_text(self.group_name.as_str()),
        );
        // Editing the field clears a previously shown error so it does not linger over text
        // the user is actively correcting (mirrors the create row's error lifecycle).
        if response.changed() {
            self.rename_error = None;
        }
        let submitted =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if let Some(err) = &self.rename_error {
            let color = ui.visuals().error_fg_color;
            ui.colored_label(color, err.as_str());
        }
        submitted
    }

    /// Renders the window's single "Применить" button and returns whether it was clicked.
    ///
    /// It is the ONLY control that writes a rename or an alias edit to the store, and it is
    /// disabled while every buffer still matches the store — so "there is something unsaved"
    /// is readable from the button alone. Both states explain themselves on hover.
    fn draw_apply_row(&self, ui: &mut egui::Ui, members: &[VirtualFontGroupMemberInfo]) -> bool {
        let dirty = self.has_pending_changes(members);
        let response = ui.add_enabled(
            dirty,
            egui::Button::new(t!("typing.font_settings.properties_apply_button")),
        );
        let response = if dirty {
            response.on_hover_text(t!("typing.font_settings.group_apply_hint"))
        } else {
            response.on_disabled_hover_text(t!("typing.font_settings.group_apply_no_changes_hint"))
        };
        response.clicked()
    }

    /// Whether any buffered edit differs from what the store currently holds — the enabled
    /// state of the "Применить" button.
    ///
    /// A member whose alias buffer was dropped (it was just removed from the group) is not
    /// counted: `members` is the store's own member list, so a buffer with no member behind it
    /// can never make the window look dirty.
    fn has_pending_changes(&self, members: &[VirtualFontGroupMemberInfo]) -> bool {
        if self.rename_buf.trim() != self.group_name {
            return true;
        }
        members.iter().any(|member| {
            self.alias_bufs
                .get(member.font.as_str())
                .is_some_and(|buf| alias_differs(buf, member.alias.as_deref()))
        })
    }

    /// Commits every buffered edit in one go: first each CHANGED alias, then the rename.
    ///
    /// Aliases go first ON PURPOSE — they are addressed by the group's CURRENT name, so a
    /// rejected rename (blank name, collision) still leaves them applied instead of silently
    /// dropping them, and a successful rename does not have to be undone to find them.
    /// Rename validation and its error reporting are unchanged; see [`Self::apply_rename`].
    fn apply_changes(
        &mut self,
        members: &[VirtualFontGroupMemberInfo],
        folder_group_names: &[String],
    ) {
        let group_name = self.group_name.clone();
        for member in members {
            let Some(buf) = self.alias_bufs.get(member.font.as_str()) else {
                continue;
            };
            if !alias_differs(buf, member.alias.as_deref()) {
                continue;
            }
            let trimmed = buf.trim();
            // Blank clears the alias (reset to the font's own label).
            let value = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            font_admin::set_virtual_group_member_alias(&group_name, &member.font, value);
        }
        self.apply_rename(folder_group_names);
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

    /// Seeds an alias buffer for every current member and DROPS every buffer that no longer
    /// belongs to one.
    ///
    /// The drop is what keeps a pending edit from outliving its member: buffers are keyed by
    /// font IDENTITY, so a stale one could never be applied to a different row, but it would
    /// come back to life the moment the same font was added to the group again. `members` is
    /// the store's own list, so a member removed from ANY surface is cleaned up here too.
    fn sync_alias_bufs(&mut self, members: &[VirtualFontGroupMemberInfo]) {
        let identities: HashSet<&str> = members.iter().map(|member| member.font.as_str()).collect();
        self.alias_bufs
            .retain(|identity, _| identities.contains(identity.as_str()));
        for member in members {
            self.alias_bufs
                .entry(member.font.clone())
                .or_insert_with(|| member.alias.clone().unwrap_or_default());
        }
    }

    /// Renders the virtualized member TABLE and returns whether the user pressed Enter in one
    /// of its alias fields (which the caller treats like the window's "Применить" button).
    ///
    /// Four columns, aligned across rows by [`MemberColumns`]: the font's user-facing name in
    /// its OWN typeface, its identity (PostScript name) in the interface font, the per-group
    /// alias field, and the remove button. A member whose font is not currently loaded shows
    /// both names greyed, falling back to the stored identity, and is never auto-removed.
    ///
    /// Own-typeface registration is bounded by the shared `preview_families` cap and only runs
    /// for VISIBLE rows. The alias field only writes to its BUFFER; the removal is the one
    /// immediate store mutation, and it is deferred until after the scroll closure so nothing
    /// mutates mid-iteration.
    fn draw_members(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        resolver: &MemberResolver,
    ) -> bool {
        self.sync_alias_bufs(members);
        ui.label(tf!(
            "typing.font_settings.group_members_header",
            count = members.len()
        ));
        if members.is_empty() {
            ui.small(t!("typing.font_settings.group_no_members_hint"));
            return false;
        }

        let group_name = self.group_name.clone();
        let mut member_to_remove: Option<String> = None;
        let mut submitted = false;

        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        // Own-typeface names can be taller than the default body; keep the alias field's
        // headroom as a floor so short-lined fonts still lay out cleanly.
        let row_height = MEMBER_ROW_HEIGHT.max(body_size * PREVIEW_ROW_HEIGHT_FACTOR);
        // Reserve the scroll bar's width up front: the header is drawn OUTSIDE the scroll area
        // and must use the same column widths as the rows inside it, which only have the
        // remaining width to spend.
        let available = ui.available_width() - ui.spacing().scroll.allocated_width();
        let columns = MemberColumns::new(available, ui.spacing().item_spacing.x, row_height);
        Self::draw_member_header(ui, &columns);

        egui::ScrollArea::vertical()
            .id_salt("typing.font_settings.group_members_list")
            .max_height(MEMBER_LIST_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, members.len(), |ui, range| {
                // `show_rows` hands us only the visible slice; `start_row` tells the grid which
                // ABSOLUTE row that slice begins at, so its per-row bookkeeping tracks the list
                // rather than the slice (`egui-0.35.0/src/grid.rs:404-410`).
                egui::Grid::new("typing.font_settings.group_members_grid")
                    .num_columns(MEMBER_TABLE_COLUMNS)
                    .min_row_height(row_height)
                    .start_row(range.start)
                    .show(ui, |ui| {
                        for row in range {
                            let Some(member) = members.get(row) else {
                                continue;
                            };
                            submitted |= self.draw_member_row(
                                ui,
                                member,
                                resolver,
                                &columns,
                                body_size,
                                &mut member_to_remove,
                            );
                            ui.end_row();
                        }
                    });
            });

        if let Some(identity) = member_to_remove {
            font_admin::remove_virtual_group_member(&group_name, &identity);
            // Drop the removed member's alias buffer with it, so an uncommitted edit cannot
            // survive its row (see [`Self::sync_alias_bufs`]).
            self.alias_bufs.remove(&identity);
        }
        submitted
    }

    /// Renders the member table's header row, using the SAME column widths as the body rows.
    ///
    /// It lives outside the virtualized scroll area (a `show_rows` grid cannot carry a
    /// non-scrolling row), so alignment relies entirely on both sides deriving their geometry
    /// from one [`MemberColumns`]. The remove column has no caption.
    fn draw_member_header(ui: &mut egui::Ui, columns: &MemberColumns) {
        let height = egui::TextStyle::Body.resolve(ui.style()).size;
        ui.horizontal(|ui| {
            for (width, caption) in [
                (
                    columns.name,
                    t!("typing.font_settings.group_member_col_name"),
                ),
                (
                    columns.identity,
                    t!("typing.font_settings.group_member_col_identity"),
                ),
                (
                    columns.alias,
                    t!("typing.font_settings.group_member_col_alias"),
                ),
            ] {
                table_cell(ui, width, height, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(caption).strong()).truncate());
                });
            }
        });
    }

    /// Renders the four cells of one member row and returns whether Enter was pressed in its
    /// alias field. A remove click is reported through `member_to_remove` so the caller can
    /// mutate the store after the iteration.
    ///
    /// `body_size` is the interface body text size, used as the own-typeface preview size.
    fn draw_member_row(
        &mut self,
        ui: &mut egui::Ui,
        member: &VirtualFontGroupMemberInfo,
        resolver: &MemberResolver,
        columns: &MemberColumns,
        body_size: f32,
        member_to_remove: &mut Option<String>,
    ) -> bool {
        let row_font = resolver.get(member.font.as_str());
        // Only an UNRESOLVED row consults the display-name override store: for a loaded font
        // the override is already baked into its display label. The lookup is an in-memory
        // read (no I/O) and runs for the handful of rows the virtualizer actually draws.
        let display_override = match row_font {
            None => font_admin::display_name_override(member.font.trim()),
            Some(_) => None,
        };
        let custom_name = member_row_name(
            FontNameDisplayMode::Custom,
            member.font.as_str(),
            row_font,
            display_override.as_deref(),
        );
        let identity_name = member_row_name(
            FontNameDisplayMode::Identity,
            member.font.as_str(),
            row_font,
            None,
        );

        // Column 1 — the user-facing name, painted in the member font's own typeface.
        table_cell(ui, columns.name, columns.row_height, |ui| {
            match row_font {
                Some(row_font) => {
                    // Registered on first VISIBLE use and bounded by the shared cap. The
                    // override is set on the CELL's own `Ui`, which is dropped with the cell,
                    // so there is nothing to restore afterwards.
                    if let Some(font_id) = own_typeface_font_id(
                        ui.ctx(),
                        member.font.as_str(),
                        row_font.content_hash,
                        row_font.path.as_path(),
                        row_font.rep_face,
                        body_size,
                        &mut self.preview_families,
                    ) {
                        ui.style_mut().override_font_id = Some(font_id);
                    }
                    ui.add(egui::Label::new(custom_name.as_str()).truncate())
                        // A name too long for its column is truncated; the hover always
                        // carries the whole of it.
                        .on_hover_text(custom_name.as_str());
                }
                None => {
                    // Keep the entry; just flag that this font is not currently loaded (do NOT
                    // auto-remove it). The stored identity is the clue the user needs, so it
                    // goes into the hover whenever the shown name is something else (a
                    // surviving display-name override).
                    let mut hint =
                        t!("typing.font_settings.group_member_missing_hint").to_string();
                    if custom_name != member.font {
                        hint.push('\n');
                        hint.push_str(member.font.as_str());
                    }
                    ui.add(
                        egui::Label::new(egui::RichText::new(custom_name.as_str()).weak())
                            .truncate(),
                    )
                    .on_hover_text(hint);
                }
            }
        });

        // Column 2 — the identity (PostScript name), always in the interface font: it is the
        // name documents store, and reading it must not depend on the font being installed.
        table_cell(ui, columns.identity, columns.row_height, |ui| {
            let mut text = egui::RichText::new(identity_name.as_str());
            if row_font.is_none() {
                text = text.weak();
            }
            ui.add(egui::Label::new(text).truncate())
                .on_hover_text(identity_name.as_str());
        });

        // Column 3 — the per-group alias BUFFER. Nothing here reaches the store until the
        // window's "Применить" button (or Enter, reported back to the caller).
        let alias_width = columns.alias;
        let submitted = table_cell(ui, alias_width, columns.row_height, |ui| {
            let Some(buf) = self.alias_bufs.get_mut(member.font.as_str()) else {
                // `sync_alias_bufs` seeds every current member, so this is unreachable; draw
                // nothing rather than panic if the invariant is ever broken.
                return false;
            };
            let response = ui.add(
                egui::TextEdit::singleline(buf)
                    .id_salt((
                        "typing.font_settings.group_member_alias_edit",
                        member.font.as_str(),
                    ))
                    .desired_width(alias_width)
                    .hint_text(t!("typing.font_settings.group_member_alias_placeholder")),
            );
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
        });

        // Column 4 — removal, the one IMMEDIATE action in this window (a membership change,
        // not a text edit).
        table_cell(ui, columns.remove, columns.row_height, |ui| {
            if ui
                .small_button("✕")
                .on_hover_text(t!("typing.font_settings.group_member_remove_tooltip"))
                .clicked()
            {
                *member_to_remove = Some(member.font.clone());
            }
        });

        submitted
    }

    /// Renders the add-member section: a "Добавить шрифт" button that expands into a picker
    /// (search + virtualized candidate rows + confirm/cancel) over the folder and imported
    /// fonts NOT already members. Each candidate row is drawn in its OWN typeface (registered
    /// on first VISIBLE use, bounded by the shared `preview_families` cap), mirroring the
    /// system-import picker.
    ///
    /// The picker also carries the window's name-display switch, because it is now the ONLY
    /// list here that has to choose a name: the member table above shows the user-facing name
    /// and the identity in adjacent columns. The choice is written through `name_mode`; the
    /// owning widget compares around the call and persists it, so nothing to react to here.
    ///
    /// The SEARCH is deliberately mode-independent: `font_row_matches` ORs every name form a
    /// row can be shown under (identity included), so flipping the switch never hides a row
    /// the user already found.
    fn draw_add_section(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        fonts: &GroupEditorFonts<'_>,
        name_mode: &mut FontNameDisplayMode,
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

        // Candidates = folder + imported fonts that are not already members (by IDENTITY).
        let member_identities: HashSet<&str> =
            members.iter().map(|member| member.font.as_str()).collect();
        let candidates: Vec<&FontEntry> = fonts
            .folder
            .iter()
            .chain(fonts.imported.iter())
            .filter(|font| !member_identities.contains(font.render_identity_name().as_str()))
            .collect();

        ui.label(t!("typing.font_settings.group_add_font_header"));
        draw_name_mode_switch(ui, FontListKind::Group, name_mode);
        let mode = *name_mode;
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
                    &font.render_identity_name(),
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
                        let identity = font.render_identity_name();
                        let is_selected = self.add_selected.as_deref() == Some(identity.as_str());
                        // Preview the candidate in its own typeface, bounded by the shared cap.
                        let prev_override = ui.style().override_font_id.clone();
                        if let Some(font_id) = own_typeface_font_id(
                            ui.ctx(),
                            &identity,
                            font.content_hash(),
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
                                font_row_name_for_mode(mode, font.display_label(), &identity),
                            )
                            .clicked();
                        ui.style_mut().override_font_id = prev_override;
                        if clicked {
                            self.add_selected = Some(identity);
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
                if let Some(identity) = self.add_selected.clone() {
                    font_admin::add_virtual_group_member(&group_name, &identity);
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

/// Whether an alias edit BUFFER holds something other than what the store records.
///
/// Both sides are compared TRIMMED, and an absent alias is equal to an empty buffer: the
/// commit writes `None` for a blank buffer, so "  " and `None` are the same value and must not
/// make the "Применить" button light up forever.
fn alias_differs(buf: &str, stored: Option<&str>) -> bool {
    buf.trim() != stored.map(str::trim).unwrap_or_default()
}

/// Picks the text one group-member row shows for `mode`.
///
/// `identity` is the member's STORED font identity (the group's own key for it). `row_font` is
/// the loaded font that identity resolved to, or `None` when nothing currently loaded matches —
/// a font whose file was removed or an entry from a machine that has it installed. Both cases
/// reuse the two `font_settings` name selectors, so a member is named exactly like the same
/// font in the folder / imported lists:
/// - resolved → the loaded font's presentation label or its identity, per `mode`;
/// - unresolved → the stored identity, except that `Custom` mode prefers the user's
///   display-name override (`display_override`) when one survives in the font store.
///
/// Never returns an empty string: a member whose stored identity is blank (which the store does
/// not produce, but which costs nothing to guard) gets the localized unnamed-font placeholder
/// instead of an invisible row.
///
/// The per-group ALIAS is deliberately absent: it is a separate, separately-edited value.
fn member_row_name(
    mode: FontNameDisplayMode,
    identity: &str,
    row_font: Option<&MemberRowFont>,
    display_override: Option<&str>,
) -> String {
    match row_font {
        Some(row_font) => font_row_name_for_mode(mode, &row_font.display_label, identity),
        None => unavailable_row_name(mode, identity, display_override)
            .unwrap_or_else(|| t!("typing.font_settings.imported_unknown_font").to_string()),
    }
}

/// Returns the `FontId` to override the style with to preview the font `identity` (whose bytes
/// live at `path`, face `face`) in its OWN typeface, or `None` to keep the default font.
///
/// The font is IDENTIFIED by `identity` and by `content_hash` (the hash of its file's bytes,
/// `0` when unknown): the registration and the per-window preview budget are keyed by both,
/// while `path` is only where the bytes are read from on first use. The hash is what expires
/// a binding whose file was replaced — egui never re-reads registered font data.
///
/// Mirrors the import picker's registration discipline (`font_settings::draw_picker_body`):
/// the family previews only if it is already bound, already previewed this window session, or
/// still under `PICKER_PREVIEW_FONT_CAP` — egui's `add_font` never evicts, so an unbounded
/// scroll would otherwise leak font atlases. On first eligible use it asks
/// `widgets::font_preview` for the font (whose bytes are read OFF the GUI thread) and records
/// the family in `preview_families`. Returns `None` (default font) beyond the cap, while the
/// bytes are still on their way, and permanently when the font cannot be registered; never
/// panics.
fn own_typeface_font_id(
    ctx: &egui::Context,
    identity: &str,
    content_hash: u64,
    path: &Path,
    face: usize,
    body_size: f32,
    preview_families: &mut HashSet<String>,
) -> Option<egui::FontId> {
    let font_name = combo_font_family_name(identity, content_hash, face);
    let allow_own = is_font_family_bound(ctx, &egui::FontFamily::Name(font_name.clone().into()))
        || preview_families.contains(&font_name)
        || preview_families.len() < PICKER_PREVIEW_FONT_CAP;
    if !allow_own {
        return None;
    }
    let PreviewFontFamily::Ready(family) =
        request_font_family(ctx, identity, content_hash, path, face)
    else {
        return None;
    };
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

    /// A resolved member row's font data, with `label` as its presentation label.
    fn row_font(label: &str) -> MemberRowFont {
        MemberRowFont {
            display_label: label.to_string(),
            content_hash: 0,
            rep_face: 0,
            path: std::path::PathBuf::from("/nonexistent/font.ttf"),
        }
    }

    #[test]
    fn member_row_name_follows_the_window_mode() {
        let font = row_font("Мой шрифт");
        // Custom mode shows the user-facing name...
        assert_eq!(
            member_row_name(
                FontNameDisplayMode::Custom,
                "CCWildWords-Regular",
                Some(&font),
                None
            ),
            "Мой шрифт"
        );
        // ...Identity mode the PostScript name the group actually stores.
        assert_eq!(
            member_row_name(
                FontNameDisplayMode::Identity,
                "CCWildWords-Regular",
                Some(&font),
                None
            ),
            "CCWildWords-Regular"
        );
        // The internal system marker never reaches the row.
        assert_eq!(
            member_row_name(
                FontNameDisplayMode::Custom,
                "Roboto-Bold",
                Some(&row_font("Roboto-Bold [system]")),
                None
            ),
            "Roboto-Bold"
        );
    }

    #[test]
    fn member_row_name_handles_a_font_that_is_not_loaded() {
        // No resolver entry (file gone / not installed here): both modes fall back to the
        // stored identity rather than panicking or rendering blank.
        for mode in [FontNameDisplayMode::Custom, FontNameDisplayMode::Identity] {
            assert_eq!(
                member_row_name(mode, "CCWildWords-Regular", None, None),
                "CCWildWords-Regular"
            );
        }
        // A surviving display-name override is preferred in Custom mode only.
        assert_eq!(
            member_row_name(
                FontNameDisplayMode::Custom,
                "CCWildWords-Regular",
                None,
                Some("Мой шрифт")
            ),
            "Мой шрифт"
        );
        assert_eq!(
            member_row_name(
                FontNameDisplayMode::Identity,
                "CCWildWords-Regular",
                None,
                Some("Мой шрифт")
            ),
            "CCWildWords-Regular"
        );
        // A blank stored identity still yields a visible row.
        assert_eq!(
            member_row_name(FontNameDisplayMode::Identity, "   ", None, None),
            t!("typing.font_settings.imported_unknown_font")
        );
    }

    #[test]
    fn add_picker_search_is_independent_of_the_mode() {
        // The picker rows follow the mode, but the predicate ORs every name form, so a font
        // stays findable by its rename AND by the identity an `Identity`-mode row shows.
        for query in ["мой", "ccwildwords", "wildwords"] {
            assert!(
                font_row_matches(
                    "wildwords",
                    "CC Wild Words",
                    "Мой шрифт",
                    "CCWildWords-Regular",
                    query
                ),
                "query {query:?} must match regardless of the display mode"
            );
        }
    }

    /// One group member with the store-recorded alias `alias`.
    fn member(identity: &str, alias: Option<&str>) -> VirtualFontGroupMemberInfo {
        VirtualFontGroupMemberInfo {
            font: identity.to_string(),
            alias: alias.map(str::to_string),
        }
    }

    #[test]
    fn alias_differs_treats_blank_and_absent_as_equal() {
        assert!(!alias_differs("", None), "empty buffer == no stored alias");
        assert!(!alias_differs("   ", None), "blank buffer == no stored alias");
        assert!(
            !alias_differs("  Жирный  ", Some("Жирный")),
            "surrounding whitespace is not an edit"
        );
        assert!(alias_differs("Жирный", None), "a new alias is an edit");
        assert!(alias_differs("", Some("Жирный")), "clearing is an edit");
        assert!(alias_differs("Тонкий", Some("Жирный")), "a rewrite is an edit");
    }

    #[test]
    fn apply_button_is_disabled_until_something_actually_changed() {
        let members = vec![member("A-Regular", None), member("B-Bold", Some("Жирный"))];
        let mut ed = editor("Экшн");
        ed.sync_alias_bufs(&members);
        assert!(
            !ed.has_pending_changes(&members),
            "buffers seeded from the store are not pending changes"
        );

        // Whitespace-only differences must not arm the button either.
        ed.rename_buf = "  Экшн  ".to_string();
        if let Some(buf) = ed.alias_bufs.get_mut("B-Bold") {
            *buf = " Жирный ".to_string();
        }
        assert!(!ed.has_pending_changes(&members));

        ed.rename_buf = "Экшн!".to_string();
        assert!(ed.has_pending_changes(&members), "a rename arms the button");

        ed.rename_buf = "Экшн".to_string();
        if let Some(buf) = ed.alias_bufs.get_mut("A-Regular") {
            *buf = "Обычный".to_string();
        }
        assert!(
            ed.has_pending_changes(&members),
            "an alias edit alone arms the button"
        );
    }

    #[test]
    fn one_apply_commits_the_rename_and_every_changed_alias() {
        let _lock = font_admin::test_lock();
        font_admin::test_reset();
        assert!(font_admin::create_virtual_group("Экшн"));
        for identity in ["A-Regular", "B-Bold", "C-Italic"] {
            assert!(font_admin::add_virtual_group_member("Экшн", identity));
        }
        let members = font_admin::list_virtual_groups().remove(0).members;

        let mut ed = editor("Экшн");
        ed.sync_alias_bufs(&members);
        ed.rename_buf = "Экшн 2".to_string();
        if let Some(buf) = ed.alias_bufs.get_mut("A-Regular") {
            *buf = "Обычный".to_string();
        }
        if let Some(buf) = ed.alias_bufs.get_mut("B-Bold") {
            *buf = "  Жирный  ".to_string();
        }
        ed.apply_changes(&members, &[]);

        assert_eq!(ed.group_name, "Экшн 2", "the single apply renamed the group");
        assert_eq!(ed.rename_error, None);
        let groups = font_admin::list_virtual_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Экшн 2");
        let aliases: Vec<(String, Option<String>)> = groups[0]
            .members
            .iter()
            .map(|member| (member.font.clone(), member.alias.clone()))
            .collect();
        assert_eq!(
            aliases,
            vec![
                ("A-Regular".to_string(), Some("Обычный".to_string())),
                // Trimmed on the way in, like the field's own placeholder promises.
                ("B-Bold".to_string(), Some("Жирный".to_string())),
                // Untouched buffer -> untouched member.
                ("C-Italic".to_string(), None),
            ],
            "one click applied BOTH changed aliases and left the third alone"
        );
        font_admin::test_reset();
    }

    #[test]
    fn a_rejected_rename_still_commits_the_alias_edits() {
        let _lock = font_admin::test_lock();
        font_admin::test_reset();
        assert!(font_admin::create_virtual_group("Экшн"));
        assert!(font_admin::add_virtual_group_member("Экшн", "A-Regular"));
        let members = font_admin::list_virtual_groups().remove(0).members;

        let mut ed = editor("Экшн");
        ed.sync_alias_bufs(&members);
        ed.rename_buf = "manga".to_string();
        if let Some(buf) = ed.alias_bufs.get_mut("A-Regular") {
            *buf = "Обычный".to_string();
        }
        // "Manga" is a real FOLDER group: the rename half must be rejected...
        ed.apply_changes(&members, &["Manga".to_string()]);
        assert_eq!(
            ed.rename_error,
            Some(t!("typing.font_settings.group_name_taken_error").to_string())
        );
        assert_eq!(ed.group_name, "Экшн");
        // ...while the alias half, addressed by the group's unchanged name, still landed.
        let groups = font_admin::list_virtual_groups();
        assert_eq!(groups[0].members[0].alias.as_deref(), Some("Обычный"));
        font_admin::test_reset();
    }

    #[test]
    fn removing_a_member_drops_its_pending_alias_edit() {
        let members = vec![member("A-Regular", None), member("B-Bold", None)];
        let mut ed = editor("Экшн");
        ed.sync_alias_bufs(&members);
        if let Some(buf) = ed.alias_bufs.get_mut("B-Bold") {
            *buf = "Жирный".to_string();
        }
        assert!(ed.has_pending_changes(&members));

        // The remove button drops the buffer with the row (the store call itself is the
        // caller's; here we assert the buffer bookkeeping the row depends on).
        ed.alias_bufs.remove("B-Bold");
        let remaining = vec![member("A-Regular", None)];
        ed.sync_alias_bufs(&remaining);
        assert!(
            !ed.alias_bufs.contains_key("B-Bold"),
            "the removed member's buffer must not linger"
        );
        assert_eq!(
            ed.alias_bufs.get("A-Regular").map(String::as_str),
            Some(""),
            "the surviving member keeps its OWN buffer; the dropped edit did not slide onto it"
        );
        assert!(
            !ed.has_pending_changes(&remaining),
            "nothing is pending once the edited member is gone"
        );
    }

    #[test]
    fn sync_alias_bufs_prunes_members_removed_elsewhere() {
        // A member deleted from another surface (the per-font properties window) leaves the
        // store; its buffer must go with it even though this window never touched the row.
        let mut ed = editor("Экшн");
        ed.sync_alias_bufs(&[member("A-Regular", None), member("B-Bold", Some("Жирный"))]);
        assert_eq!(ed.alias_bufs.len(), 2);
        assert_eq!(
            ed.alias_bufs.get("B-Bold").map(String::as_str),
            Some("Жирный"),
            "a stored alias seeds the buffer"
        );
        ed.sync_alias_bufs(&[member("A-Regular", None)]);
        assert_eq!(ed.alias_bufs.len(), 1);
        assert!(!ed.alias_bufs.contains_key("B-Bold"));
    }

    #[test]
    fn a_member_whose_font_is_not_loaded_fills_both_name_columns() {
        // Nothing resolves the identity (the file is gone), so BOTH table columns fall back to
        // what the document records instead of rendering blank or panicking.
        let identity = "CCWildWords-Regular";
        assert_eq!(
            member_row_name(FontNameDisplayMode::Custom, identity, None, None),
            identity,
            "the name column falls back to the stored identity"
        );
        assert_eq!(
            member_row_name(FontNameDisplayMode::Identity, identity, None, None),
            identity,
            "the identity column shows the stored identity"
        );
        // A surviving display-name override is what the name column prefers.
        assert_eq!(
            member_row_name(FontNameDisplayMode::Custom, identity, None, Some("Мой шрифт")),
            "Мой шрифт"
        );
        assert_eq!(
            member_row_name(FontNameDisplayMode::Identity, identity, None, Some("Мой шрифт")),
            identity,
            "the identity column is never replaced by a rename"
        );
        // Even a blank stored identity leaves both columns visible.
        for mode in [FontNameDisplayMode::Custom, FontNameDisplayMode::Identity] {
            assert_eq!(
                member_row_name(mode, "  ", None, None),
                t!("typing.font_settings.imported_unknown_font")
            );
        }
    }

    #[test]
    fn name_columns_split_the_leftover_width_and_stop_at_the_floor() {
        let spacing = 8.0;
        let fixed = ALIAS_EDIT_WIDTH + REMOVE_COL_WIDTH + 3.0 * spacing;
        // Wide window: both name columns share everything the fixed columns left over.
        let wide = name_column_width(fixed + 400.0, spacing);
        assert!(
            (wide - 200.0).abs() < f32::EPSILON,
            "expected an even split, got {wide}"
        );
        // Narrow window: the columns stop shrinking instead of collapsing to nothing.
        assert!(
            (name_column_width(fixed, spacing) - MIN_NAME_COL_WIDTH).abs() < f32::EPSILON
        );
        assert!((name_column_width(0.0, spacing) - MIN_NAME_COL_WIDTH).abs() < f32::EPSILON);
        // The floor is what `EDITOR_WINDOW_MIN_WIDTH` is sized around: at that width the table
        // still fits, so the remove buttons cannot be resized out of reach.
        assert!(2.0 * MIN_NAME_COL_WIDTH + fixed <= EDITOR_WINDOW_MIN_WIDTH);
        // Both name columns are always equal, so the header cannot drift from the rows.
        let columns = MemberColumns::new(600.0, spacing, 30.0);
        assert!((columns.name - columns.identity).abs() < f32::EPSILON);
        assert!((columns.alias - ALIAS_EDIT_WIDTH).abs() < f32::EPSILON);
        assert!((columns.remove - REMOVE_COL_WIDTH).abs() < f32::EPSILON);
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
