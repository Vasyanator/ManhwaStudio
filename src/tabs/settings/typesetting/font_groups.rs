/*
File: settings/typesetting/font_groups.rs

Purpose:
The "Группы" section of the settings "Настройки шрифтов" block: create, list, rename,
delete VIRTUAL font groups (user-defined named sets of real fonts) and edit each group's
members and per-group display aliases. UI ONLY — the group MODEL lives in
`crate::tabs::typing` and is reached exclusively through the `font_admin` facade.

Main responsibilities:
- render the create row (name field + validation against existing virtual groups AND real
  folder-group names) and the group list with an inline two-step delete confirm; a group that
  references a font which is not currently loaded has its row name painted in the WARNING
  color, so a broken group is visible without opening it;
- own the floating group-editor window (`GroupEditorState`): a rename field, a virtualized
  member TABLE (own-typeface name / identity / per-group alias / missing flag / remove), an
  inline add-member picker mirroring the system-font import picker body, and an ADAPTIVE bottom
  row: the two bulk-import buttons on the left and the single "Применить" button on the right,
  which commits the rename and every changed alias together — split over two lines when the
  measured captions do not fit the available width;
- run the FONT-CARD import: a PSD whose text layers are set in the fonts they name is read on
  a worker thread (`font_card_psd::read_font_card`), each name resolved against the loaded
  fonts (and, failing that, against the OS-installed fonts, which are then auto-imported), and
  the whole card is committed to the edited group as ONE batch with the layer texts as the
  members' per-group aliases;
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
- `CardImportOutcome` / `ResolvedCardEntry` / `CardImportReport` (the font-card import pipeline:
  what the worker produces, and the counts the status line reports)

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

IDENTITY COMPARISON: every identity this module compares, keys or looks up goes through
`font_admin::normalize_font_identity` (trim + ASCII lowercase) — the loaded-identity set, the
member resolver, the add-picker's "already a member" filter, the card deduplication and the
card name resolution alike. That is the rule the typing panel's `apply_virtual_groups` and the
store both use, and documents legitimately carry an identity in another casing (the deferred
legacy-key migration leaves such keys alone on purpose), so a byte-exact comparison here would
mark as "Отсутствует" a member the panel resolves fine. The synthetic BUNDLED interface font is
the one available font that is in NEITHER category — `font_admin::is_bundled_ui_font_identity`
is what keeps it from being reported missing (see `member_is_available`).

FONT-CARD IMPORT THREADING: the click only SPAWNS a worker. The native file picker, the PSD
read and every by-name system-font lookup (a cold one scans the whole OS font database) run
there; the GUI polls one `mpsc` result with `try_recv` and schedules the next poll with
`request_repaint_after` (the picker is modal and unbounded in time, so a full-rate repaint loop
would burn the whole app's frame budget on waiting). The worker gets a SNAPSHOT of the loaded
identities, never a borrow of the font lists. Only the two store writes (`add_imported_fonts`,
then `add_virtual_group_members` — one batch each, so the whole card costs ONE revision bump
apiece) happen on the GUI thread, where they are in-memory. If the edited group is gone by the
time the result lands, the system fonts are still imported but the membership batch is skipped
and the status line reports an ERROR, not a "0 added" report.
*/

use super::font_card_psd::{FontCardEntry, FontCardError, read_font_card};
use super::font_settings::{
    FontListKind, FontNameDisplayMode, PICKER_PREVIEW_FONT_CAP, PREVIEW_ROW_HEIGHT_FACTOR,
    draw_name_mode_switch, font_row_matches, font_row_name_for_mode, unavailable_row_name,
};
use crate::runtime_log;
use crate::tabs::typing::font_admin::{self, FontEntry, VirtualFontGroupInfo, VirtualFontGroupMemberInfo};
use crate::widgets::{
    PreviewFontFamily, combo_font_family_name, is_font_family_bound, request_font_family,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
/// Width (points) of the member table's "missing font" flag column. Sized around the localized
/// badge, which is a fixed word, not user data.
const MISSING_COL_WIDTH: f32 = 104.0;
/// Smallest width (points) either NAME column of the member table may shrink to. Below this a
/// truncated name carries no information at all, so the table is allowed to overflow the
/// window instead (the window's own `min_width` keeps that from happening in practice).
const MIN_NAME_COL_WIDTH: f32 = 90.0;
/// Columns of the member table: font name, identity, per-group alias, missing flag, remove
/// button.
const MEMBER_TABLE_COLUMNS: usize = 5;
/// How often the GUI re-checks the in-flight font-card import result channel.
///
/// The worker's FIRST step is the native file dialog, which the user may leave open for
/// minutes; an unconditional `request_repaint()` per poll would hold the whole app at full
/// frame rate for that entire time. 150 ms is well under the delay a user can notice between
/// confirming the dialog and seeing the status line, and costs ~7 frames per second instead.
const CARD_IMPORT_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Separator between a base font identity and its content-hash collision suffix, mirroring
/// the typing model's `fonts::IDENTITY_HASH_SEPARATOR` (a character the PostScript-name spec
/// forbids, so a suffixed identity can never collide with a real name). Only [`identity_base`]
/// needs it: a font card records the BARE PostScript name, so a suffixed identity has to be
/// folded back to its base before the card's name can match it.
const IDENTITY_HASH_SEPARATOR: char = '%';
/// Minimum window width (points) that keeps the member table usable: both name columns at
/// their floor, the three fixed columns and the four inter-column gaps at the default item
/// spacing, plus the window frame margins and headroom for the table's own hovers.
///
/// It is sized around the TABLE only. The bottom row no longer depends on it: its buttons are
/// measured and the row splits itself over two lines when they do not fit
/// ([`bottom_row_is_stacked`]), which is what makes it survive a longer locale or a larger
/// interface font — widths a hand-picked constant cannot anticipate.
const EDITOR_WINDOW_MIN_WIDTH: f32 = 660.0;
/// Whether the font-card import can actually run in this build.
///
/// `rfd` is a desktop-only dependency: the web build links no native file dialog, the worker's
/// pick resolves as "cancelled", and the button would silently do nothing. It is disabled there
/// with an explaining hover instead — see `GroupEditorState::draw_import_buttons`.
#[cfg(not(target_arch = "wasm32"))]
const CARD_IMPORT_AVAILABLE: bool = true;
/// Web build: see the non-wasm definition.
#[cfg(target_arch = "wasm32")]
const CARD_IMPORT_AVAILABLE: bool = false;

/// Member-name resolver: NORMALIZED font IDENTITY → the data a member row needs to draw itself.
///
/// The KEY is the loaded font's `FontEntry::render_identity_name` run through
/// [`font_admin::normalize_font_identity`], and a lookup must normalize the stored member
/// identity the same way. The identity is what `fonts_data.json` records, so a member keeps
/// resolving after its file is moved or renamed; normalizing is what keeps a member persisted
/// in a different CASING resolving too — the typing panel's group merge folds case, so a
/// byte-exact lookup here would call "missing" a member the panel happily shows.
///
/// A key that resolves to nothing is a font that is not currently loaded (or a stale legacy
/// reference an unmigrated document still carries); its row is shown greyed and is never
/// auto-removed. The synthetic BUNDLED interface font is deliberately absent from the map (it
/// is in neither font category) — see [`member_is_available`].
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
/// The five columns are laid out with EXPLICIT widths rather than left to `egui::Grid`'s
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
    /// Width of the "missing font" flag column (empty for a member whose font IS loaded).
    missing: f32,
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
            missing: MISSING_COL_WIDTH,
            remove: REMOVE_COL_WIDTH,
            row_height,
        }
    }
}

/// Width one NAME column of the member table gets for `available_width`.
///
/// The alias field, the missing-flag column and the remove button keep fixed widths; whatever
/// is left after them and the four inter-column gaps (`spacing_x` each) is split evenly between
/// the two name columns, so widening the window widens the names. The result never drops below
/// [`MIN_NAME_COL_WIDTH`], in which case the table simply overflows the window.
fn name_column_width(available_width: f32, spacing_x: f32) -> f32 {
    let fixed = fixed_columns_width(spacing_x);
    ((available_width - fixed) / 2.0).max(MIN_NAME_COL_WIDTH)
}

/// Total width the member table spends on everything that is NOT a name column: the three
/// fixed columns plus the four inter-column gaps of `spacing_x` each.
///
/// Shared by [`name_column_width`] and the window's minimum-width assertion so the two cannot
/// drift when a column is added or resized.
fn fixed_columns_width(spacing_x: f32) -> f32 {
    ALIAS_EDIT_WIDTH + MISSING_COL_WIDTH + REMOVE_COL_WIDTH + 4.0 * spacing_x
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

/// Extra width (points) added to each measured button caption before the bottom row's layout is
/// decided.
///
/// [`button_width`] reconstructs egui's own sizing from the caption galley and
/// `Spacing::button_padding`, but the real frame margin also carries the widget's expansion and
/// its border stroke (`egui-0.35.0/src/widget_style.rs:161-165`), both of which vary with the
/// interaction state. The slack keeps the estimate on the safe side: erring high stacks the row
/// a few points early, erring low clips a caption — the defect this measurement exists to
/// prevent.
const BUTTON_WIDTH_SLACK: f32 = 4.0;

/// Width (points) `label` needs as a plain `egui::Button` in the CURRENT style.
///
/// Text LAYOUT only — no painting and no I/O — so it is safe on the GUI thread, and egui caches
/// galleys, so re-measuring a fixed caption every frame costs a lookup. The result is the
/// caption's laid-out width plus the style's horizontal button padding on both sides plus
/// [`BUTTON_WIDTH_SLACK`].
fn button_width(ui: &egui::Ui, label: &str) -> f32 {
    let font_id = egui::TextStyle::Button.resolve(ui.style());
    // The colour is irrelevant to the measurement; the galley is never painted.
    let text_width = ui.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.to_string(), font_id, egui::Color32::WHITE)
            .size()
            .x
    });
    text_width + ui.spacing().button_padding.x * 2.0 + BUTTON_WIDTH_SLACK
}

/// Whether the group editor's bottom row must be split over TWO lines.
///
/// Pure geometry, so the rule is unit-tested instead of eyeballed against one locale:
/// `imports_width` is what the two bulk-import buttons need together (both captions plus the
/// gap between them), `apply_width` what "Применить" needs, `spacing_x` the gap that would
/// separate the two halves, and `available_width` what the row may occupy.
///
/// `egui::Ui::horizontal` does NOT wrap: once the three buttons stop fitting, the right-aligned
/// one is squeezed and its caption clipped. That is exactly when the import buttons move to a
/// line of their own, above the apply row.
fn bottom_row_is_stacked(
    imports_width: f32,
    apply_width: f32,
    spacing_x: f32,
    available_width: f32,
) -> bool {
    imports_width + spacing_x + apply_width > available_width
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
    /// Identities of the currently loaded fonts, keyed by the categories snapshot revision they
    /// were collected from. Rebuilt only when that revision advances: the group list asks it
    /// once per group per frame, and rebuilding it per row would be O(groups × fonts).
    loaded_identities: Option<(u64, HashSet<String>)>,
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
                self.draw_group_list(ui, fonts);
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
    ///
    /// A group holding at least one member whose font is NOT currently loaded is named in the
    /// WARNING color with an explaining hover: those members are silently dropped from the
    /// typing panel's font list, so the group behaves as if it were smaller than it looks and
    /// the user needs a clue before opening it. `fonts` supplies the loaded identities, cached
    /// on `self` per snapshot revision (see [`Self::loaded_identity_set`]).
    fn draw_group_list(&mut self, ui: &mut egui::Ui, fonts: &GroupEditorFonts<'_>) {
        if self.groups.is_empty() {
            ui.small(t!("typing.font_settings.groups_empty_hint"));
            return;
        }
        // Move the snapshot AND the identity cache out so the row closures can mutate `self`
        // (arm delete, open the editor) without aliasing either; both are restored afterward.
        let groups = std::mem::take(&mut self.groups);
        let loaded = self.loaded_identity_set(fonts);
        for group in &groups {
            ui.horizontal(|ui| {
                let label = tf!(
                    "typing.font_settings.group_row_label",
                    name = group.name,
                    count = group.members.len()
                );
                if group_has_missing_font(group, &loaded) {
                    let color = ui.visuals().warn_fg_color;
                    ui.label(egui::RichText::new(label).color(color))
                        .on_hover_text(t!("typing.font_settings.group_missing_fonts_hint"));
                } else {
                    ui.label(label);
                }
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
        self.loaded_identities = Some((fonts.categories_revision, loaded));
    }

    /// Takes the cached set of loaded font identities, rebuilding it from `fonts` when the
    /// cached snapshot revision is stale or nothing is cached yet.
    ///
    /// The set is MOVED OUT of `self` (the caller restores it) so the group rows can keep
    /// mutating `self` while reading it. It holds each loaded font's
    /// `FontEntry::render_identity_name` run through [`font_admin::normalize_font_identity`],
    /// exactly like the editor window's member resolver, so both surfaces call a member
    /// "missing" under the same rule — and the same rule the typing panel's group merge uses.
    /// Look a member up with [`member_is_available`], never with a raw `contains`.
    fn loaded_identity_set(&mut self, fonts: &GroupEditorFonts<'_>) -> HashSet<String> {
        match self.loaded_identities.take() {
            Some((revision, set)) if revision == fonts.categories_revision => set,
            Some(_) | None => fonts
                .folder
                .iter()
                .chain(fonts.imported.iter())
                .map(|font| font_admin::normalize_font_identity(&font.render_identity_name()))
                .collect(),
        }
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
            card_import_rx: None,
            card_import_status: None,
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
                // Keyed NORMALIZED (see `MemberResolver`): a member persisted in another
                // casing must resolve to the loaded font, exactly as it does in the panel.
                map.entry(font_admin::normalize_font_identity(
                    &font.render_identity_name(),
                ))
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
            .default_size([700.0, 560.0])
            // The member table has a minimum width of its own (two name columns at their floor
            // plus the three fixed ones); stop the user from resizing the window narrower,
            // which would push the remove buttons out of reach. The bottom row protects itself
            // by measuring its buttons and stacking when they do not fit.
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
    /// Receiver of the in-flight font-card import (picker + PSD read + system-font lookups run
    /// on a worker thread). `Some` means an import is in flight and the button is disabled.
    card_import_rx: Option<mpsc::Receiver<CardImportOutcome>>,
    /// Result of the LAST font-card import, shown under the bottom row until the next import
    /// starts. A cancelled pick leaves it untouched (cancelling is not an outcome to report).
    card_import_status: Option<CardImportStatus>,
}

/// What the bottom row reports about the last font-card import.
#[derive(Debug, Clone)]
enum CardImportStatus {
    /// Localized counts of what the card added (drawn in the normal small text style).
    Report(String),
    /// Localized failure message (drawn in `Visuals::error_fg_color`). The technical half of
    /// the failure went to `runtime_log` when it was produced.
    Error(String),
}

impl GroupEditorState {
    /// Renders the whole window body: rename field, the member table, the add-member section
    /// and the bottom row (bulk-import buttons on the left, the single "Применить" button that
    /// commits every buffered edit at once on the right).
    ///
    /// It also POLLS the font-card import: the result is applied against `members`, the store's
    /// own member list, so "this font was already in the group" is decided from the store and
    /// not from anything the worker saw.
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
        // Polled before anything is drawn so a finished import is reflected by THIS frame's
        // status line instead of the next one.
        self.poll_card_import(ui.ctx(), members);

        let rename_submitted = self.draw_rename_row(ui);
        ui.add_space(6.0);
        ui.separator();

        let alias_submitted = self.draw_members(ui, members, resolver);
        ui.add_space(6.0);
        ui.separator();

        self.draw_add_section(ui, members, fonts, name_mode);

        ui.add_space(6.0);
        ui.separator();
        let apply_clicked = self.draw_bottom_row(ui, members, fonts);

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

    /// Renders the window's bottom row and returns whether "Применить" was clicked.
    ///
    /// Layout is ADAPTIVE, decided by [`bottom_row_is_stacked`] from the buttons' MEASURED
    /// widths in the current style (so it holds for any locale, font size and window width):
    /// - wide enough — one row: the BULK-IMPORT buttons on the left in document order, the
    ///   "Применить" button pushed to the RIGHT edge by a nested `right_to_left` layout;
    /// - too narrow — two rows: the import buttons get a line of their own ABOVE, and the
    ///   apply row keeps its right alignment. `ui.horizontal` does not wrap, so without this
    ///   the right-aligned button would be squeezed and its caption clipped.
    ///
    /// The archive import is permanently disabled (the feature does not exist yet) and the
    /// card import is disabled on the web build (no native file dialog); both say so on hover
    /// and are drawn rather than omitted so the capability stays discoverable.
    ///
    /// Below the row, the last font-card import's status line: the counts in the normal small
    /// style, a failure in `Visuals::error_fg_color`.
    fn draw_bottom_row(
        &mut self,
        ui: &mut egui::Ui,
        members: &[VirtualFontGroupMemberInfo],
        fonts: &GroupEditorFonts<'_>,
    ) -> bool {
        let mut apply_clicked = false;
        let spacing_x = ui.spacing().item_spacing.x;
        // Measuring three galleys per frame is pure text layout against egui's own cache — no
        // I/O and no allocation of consequence — so it is fine on the GUI thread.
        let imports_width = button_width(ui, t!("typing.font_settings.group_import_card_button"))
            + spacing_x
            + button_width(ui, t!("typing.font_settings.group_import_archive_button"));
        let apply_width = button_width(ui, t!("typing.font_settings.properties_apply_button"));
        let stacked = bottom_row_is_stacked(
            imports_width,
            apply_width,
            spacing_x,
            ui.available_width(),
        );

        if stacked {
            ui.horizontal(|ui| self.draw_import_buttons(ui, fonts));
        }
        ui.horizontal(|ui| {
            if !stacked {
                self.draw_import_buttons(ui, fonts);
            }
            // Right-aligned tail of the row. Inside a right-to-left layout the first widget
            // added is the RIGHTMOST one, so "Применить" alone belongs here.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                apply_clicked = self.draw_apply_button(ui, members);
            });
        });

        match &self.card_import_status {
            Some(CardImportStatus::Report(text)) => {
                ui.small(text.as_str());
            }
            Some(CardImportStatus::Error(text)) => {
                let color = ui.visuals().error_fg_color;
                ui.colored_label(color, text.as_str());
            }
            None => {}
        }

        apply_clicked
    }

    /// Renders the two BULK-IMPORT buttons, left to right, into the row the caller opened.
    ///
    /// Both are drawn in every build and every state, disabled with an explaining hover when
    /// they cannot act, so the capability stays discoverable instead of silently vanishing:
    /// - the font-card import is disabled while one is already in flight, and permanently on
    ///   the web build, which links no native file dialog ([`CARD_IMPORT_AVAILABLE`]) — there
    ///   the click would resolve as "cancelled" and the button would do nothing at all;
    /// - the archive import is permanently disabled: the feature does not exist yet.
    fn draw_import_buttons(&mut self, ui: &mut egui::Ui, fonts: &GroupEditorFonts<'_>) {
        let importing = self.card_import_rx.is_some();
        let card = ui.add_enabled(
            CARD_IMPORT_AVAILABLE && !importing,
            egui::Button::new(t!("typing.font_settings.group_import_card_button")),
        );
        let card = if !CARD_IMPORT_AVAILABLE {
            card.on_disabled_hover_text(t!("typing.font_settings.group_import_card_web_hint"))
        } else if importing {
            card.on_disabled_hover_text(t!("typing.font_settings.group_import_card_busy_hint"))
        } else {
            card.on_hover_text(t!("typing.font_settings.group_import_card_hint"))
        };
        if card.clicked() {
            self.start_card_import(ui.ctx(), fonts);
        }
        ui.add_enabled(
            false,
            egui::Button::new(t!("typing.font_settings.group_import_archive_button")),
        )
        .on_disabled_hover_text(t!("typing.font_settings.group_import_archive_hint"));
    }

    /// Renders the window's single "Применить" button and returns whether it was clicked.
    ///
    /// It is the ONLY control that writes a rename or an alias edit to the store, and it is
    /// disabled while every buffer still matches the store — so "there is something unsaved"
    /// is readable from the button alone. Both states explain themselves on hover.
    fn draw_apply_button(&self, ui: &mut egui::Ui, members: &[VirtualFontGroupMemberInfo]) -> bool {
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

    /// Starts a font-card import on a worker thread: the native file picker, the PSD read and
    /// every by-name system-font lookup happen there, never on the GUI thread.
    ///
    /// The worker is handed a SNAPSHOT of the loaded font identities (`fonts` cannot cross the
    /// thread boundary — it is a one-frame borrow), which is what decides whether a name from
    /// the card is already in the program's font base. A refused spawn is logged and reported in
    /// the status line; the button stays enabled so the user can retry.
    ///
    /// A successful spawn requests ONE repaint: the poll that keeps the frame loop alive runs
    /// BEFORE this row is drawn, so without it the frame that started the import would be the
    /// last one, and the result would sit in the channel until the user moved the mouse.
    fn start_card_import(&mut self, ctx: &egui::Context, fonts: &GroupEditorFonts<'_>) {
        if self.card_import_rx.is_some() {
            return;
        }
        self.card_import_status = None;
        let loaded: Vec<String> = fonts
            .folder
            .iter()
            .chain(fonts.imported.iter())
            .map(FontEntry::render_identity_name)
            .collect();
        match spawn_card_import(loaded) {
            Ok(rx) => {
                self.card_import_rx = Some(rx);
                ctx.request_repaint();
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[settings] failed to start the font-card import thread; error={err}"
                ));
                self.card_import_status = Some(CardImportStatus::Error(
                    t!("typing.font_settings.group_card_import_failed").to_string(),
                ));
            }
        }
    }

    /// Polls the in-flight font-card import without blocking, applying the result when it
    /// arrives and keeping the frame loop alive while it is not.
    ///
    /// The keep-alive is a `request_repaint_after(CARD_IMPORT_POLL_INTERVAL)`, NOT a bare
    /// `request_repaint`: the worker sits in a modal native file dialog first, for an unbounded
    /// time, and repainting every frame for all of it burns CPU/GPU for nothing.
    ///
    /// `members` is the store's CURRENT member list of the edited group, used to attribute the
    /// counts of the status line.
    fn poll_card_import(&mut self, ctx: &egui::Context, members: &[VirtualFontGroupMemberInfo]) {
        let Some(rx) = self.card_import_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.card_import_rx = None;
                self.apply_card_import(outcome, members);
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(CARD_IMPORT_POLL_INTERVAL),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.card_import_rx = None;
                runtime_log::log_error(
                    "[settings] the font-card import thread ended without sending a result",
                );
                self.card_import_status = Some(CardImportStatus::Error(
                    t!("typing.font_settings.group_card_import_failed").to_string(),
                ));
            }
        }
    }

    /// Applies one finished font-card import to the store and fills the status line.
    ///
    /// Two batched writes, in this order: the fonts the worker located in the SYSTEM are
    /// imported into the program's font base (so they resolve like any other imported font from
    /// now on), then every card entry — resolved, auto-imported or missing alike — is appended
    /// to the group with the layer's text as its per-group alias. Batching matters: each store
    /// mutation bumps the shared revision, and every open font list reloads on a bump.
    ///
    /// A cancelled pick is SILENT (no status, no log): the user closed the dialog on purpose.
    /// A failure shows its localized half and logs its technical half.
    ///
    /// When the edited group is GONE by the time the result arrives (deleted here or from
    /// another surface while the worker ran), the located system fonts are STILL imported into
    /// the font base — that half of the work is independent of the group and is a real gain —
    /// but the membership batch is skipped and an ERROR is reported instead of a report. The
    /// distinction matters: `add_virtual_group_members` also returns `0` when every entry was
    /// already a member, and reporting "Добавлено: 0. Пропущено: N" for a vanished group would
    /// tell the user the card was already there. See [`virtual_group_exists`].
    fn apply_card_import(
        &mut self,
        outcome: CardImportOutcome,
        members: &[VirtualFontGroupMemberInfo],
    ) {
        match outcome {
            CardImportOutcome::Cancelled => {}
            CardImportOutcome::Failed(error) => {
                runtime_log::log_error(format!(
                    "[settings] font-card import failed; {}",
                    error.log_message
                ));
                self.card_import_status = Some(CardImportStatus::Error(error.user_message));
            }
            CardImportOutcome::Loaded {
                entries,
                duplicates,
            } => {
                let group = self.group_name.clone();
                let imports: Vec<(String, PathBuf)> = entries
                    .iter()
                    .filter_map(|entry| {
                        entry
                            .source
                            .system_path()
                            .map(|path| (entry.identity.clone(), path.to_path_buf()))
                    })
                    .collect();
                let auto_imported = if imports.is_empty() {
                    0
                } else {
                    font_admin::add_imported_fonts(&imports)
                };
                if !virtual_group_exists(&group) {
                    // The group went away while the worker ran (deleted here or from another
                    // surface). The system fonts above are kept — they are now part of the
                    // font base regardless — but there is nothing left to add them to.
                    runtime_log::log_warn(format!(
                        "[settings] font card read, but the edited virtual group '{group}' no longer exists; \
                         {auto_imported} system font(s) were imported, no members were added"
                    ));
                    self.card_import_status = Some(CardImportStatus::Error(
                        t!("typing.font_settings.group_card_import_group_gone").to_string(),
                    ));
                    return;
                }
                let batch: Vec<(String, Option<String>)> = entries
                    .iter()
                    .map(|entry| (entry.identity.clone(), Some(entry.title.clone())))
                    .collect();
                let added = font_admin::add_virtual_group_members(&group, &batch);
                let report =
                    summarize_card_import(&entries, members, duplicates, added, auto_imported);
                runtime_log::log_info(format!(
                    "[settings] font card applied to virtual group '{group}': added={} auto_imported={} missing={} invalid_names={} skipped={}",
                    report.added, report.auto_imported, report.missing, report.invalid, report.skipped
                ));
                self.card_import_status =
                    Some(CardImportStatus::Report(card_import_report_line(&report)));
            }
        }
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
    /// Five columns, aligned across rows by [`MemberColumns`]: the font's user-facing name in
    /// its OWN typeface, its identity (PostScript name) in the interface font, the per-group
    /// alias field, the "missing font" flag, and the remove button. A member whose font is not
    /// currently loaded shows both names greyed, falling back to the stored identity, carries
    /// the red badge in the flag column, and is never auto-removed.
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
    /// from one [`MemberColumns`]. Only the three leading columns are captioned: the trailing
    /// flag and remove columns carry per-row marks, not fields, and nothing follows them whose
    /// alignment an empty header cell could protect.
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

    /// Renders the five cells of one member row and returns whether Enter was pressed in its
    /// alias field. A remove click is reported through `member_to_remove` so the caller can
    /// mutate the store after the iteration.
    ///
    /// A member the typing panel will NOT show gets the red "Отсутствует" badge in the fourth
    /// cell on top of the greyed names; it is still never auto-removed. Availability follows
    /// the module's one rule: the resolver lookup is NORMALIZED, and the synthetic bundled
    /// interface font counts as available even though it has no resolver entry.
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
        // Normalized lookup: the resolver is keyed that way so a member persisted in another
        // casing resolves here exactly as it does in the typing panel.
        let row_font = resolver.get(&font_admin::normalize_font_identity(&member.font));
        // The synthetic bundled interface font is in neither font category, so it never has a
        // resolver entry — but the panel prepends it to its own list, so the member is present
        // there and must not be badged as missing.
        let bundled = row_font.is_none() && font_admin::is_bundled_ui_font_identity(&member.font);
        let missing = row_font.is_none() && !bundled;
        // Only an UNRESOLVED row consults the display-name override store: for a loaded font
        // the override is already baked into its display label. The lookup is an in-memory
        // read (no I/O) and runs for the handful of rows the virtualizer actually draws.
        let display_override = match row_font {
            None => font_admin::display_name_override(member.font.trim()),
            Some(_) => None,
        };
        let custom_name = if bundled {
            // Name the built-in font the way every other surface does (the same catalog entry
            // `FontEntry::display_label` returns for it), instead of showing the reserved
            // identity in the user-facing column.
            t!("typing.fonts.bundled_ui_font_label").to_string()
        } else {
            member_row_name(
                FontNameDisplayMode::Custom,
                member.font.as_str(),
                row_font,
                display_override.as_deref(),
            )
        };
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
                    // surviving display-name override, or the bundled font's catalog name).
                    // The bundled entry has no `FontEntry` here but IS available, so it is
                    // drawn at full strength and without the "not loaded" note.
                    let mut hint = if missing {
                        t!("typing.font_settings.group_member_missing_hint").to_string()
                    } else {
                        String::new()
                    };
                    if custom_name != member.font {
                        if !hint.is_empty() {
                            hint.push('\n');
                        }
                        hint.push_str(member.font.as_str());
                    }
                    let mut text = egui::RichText::new(custom_name.as_str());
                    if missing {
                        text = text.weak();
                    }
                    ui.add(egui::Label::new(text).truncate())
                        .on_hover_text(hint);
                }
            }
        });

        // Column 2 — the identity (PostScript name), always in the interface font: it is the
        // name documents store, and reading it must not depend on the font being installed.
        table_cell(ui, columns.identity, columns.row_height, |ui| {
            let mut text = egui::RichText::new(identity_name.as_str());
            if missing {
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

        // Column 4 — the "font is not loaded" flag. Empty for a resolved member, so the mark
        // reads as an exception rather than as a per-row status field. It repeats in colour
        // what the greyed names already hint, because grey alone does not say WHY the row looks
        // different, and this member is the one the typing panel will silently drop.
        table_cell(ui, columns.missing, columns.row_height, |ui| {
            if missing {
                let color = ui.visuals().error_fg_color;
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(t!("typing.font_settings.group_member_missing_badge"))
                            .color(color),
                    )
                    .truncate(),
                )
                .on_hover_text(t!("typing.font_settings.group_member_missing_hint"));
            }
        });

        // Column 5 — removal, the one IMMEDIATE action in this window (a membership change,
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

        // Candidates = folder + imported fonts that are not already members. Compared by
        // NORMALIZED identity, the module's one identity rule: the store refuses a duplicate
        // member the same way, so offering a font whose membership is recorded in another
        // casing would be a candidate that does nothing when picked.
        let member_identities: HashSet<String> = members
            .iter()
            .map(|member| font_admin::normalize_font_identity(&member.font))
            .collect();
        let candidates: Vec<&FontEntry> = fonts
            .folder
            .iter()
            .chain(fonts.imported.iter())
            .filter(|font| {
                !member_identities
                    .contains(&font_admin::normalize_font_identity(&font.render_identity_name()))
            })
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

/// Whether the stored member identity `identity` names a font the typing panel will actually
/// show for the group.
///
/// The SINGLE availability rule of this module, shared by the group list's warning color and
/// by the member table's "Отсутствует" badge, so the two surfaces can never disagree:
/// - `loaded` holds the NORMALIZED identities of the loaded fonts (see
///   [`FontGroupsEditorState::loaded_identity_set`]) and the member is normalized the same way
///   ([`font_admin::normalize_font_identity`]). Comparing raw strings instead would flag a
///   member persisted as `roboto-bold` while `Roboto-Bold` is loaded — a spelling the deferred
///   legacy-key migration deliberately leaves in place, and one the panel's
///   `apply_virtual_groups` resolves without trouble;
/// - the synthetic BUNDLED interface font is available too, even though it is in neither font
///   category: the panel prepends it to its own list, so a member holding its identity is
///   shown there like any other.
fn member_is_available(identity: &str, loaded: &HashSet<String>) -> bool {
    font_admin::is_bundled_ui_font_identity(identity)
        || loaded.contains(&font_admin::normalize_font_identity(identity))
}

/// Whether `group` references at least one font the typing panel will not show.
///
/// Availability is decided by [`member_is_available`], the same rule the editor window's
/// member table uses, so the group list and the member table can never disagree about which
/// member is missing. Such a member is silently dropped by the typing panel's group merge,
/// which is what the warning color in the group list is there to announce.
fn group_has_missing_font(group: &VirtualFontGroupInfo, loaded: &HashSet<String>) -> bool {
    group
        .members
        .iter()
        .any(|member| !member_is_available(&member.font, loaded))
}

/// Where a font-card entry's identity came from — and therefore what the import had to do
/// about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CardEntrySource {
    /// The name resolved to a font the program has already loaded; nothing else was needed.
    Loaded,
    /// The name matched no loaded font but IS installed in the system; its bytes live at this
    /// path and it is auto-imported into the program's font base.
    SystemImport(PathBuf),
    /// Nothing claims this name, here or in the system: the member is added as MISSING so the
    /// card's structure survives, and the user sees which font to install.
    Missing,
    /// The name is not a spec-valid PostScript name, so it cannot be a font identity at all.
    /// Added as missing like any other unresolvable name, but counted separately: it usually
    /// means the card layer was set in a font Photoshop recorded oddly, not a font to install.
    InvalidName,
}

impl CardEntrySource {
    /// The file to import the font's bytes from, or `None` when this entry needs no import.
    fn system_path(&self) -> Option<&Path> {
        match self {
            Self::SystemImport(path) => Some(path.as_path()),
            Self::Loaded | Self::Missing | Self::InvalidName => None,
        }
    }

    /// Whether the entry's font is unavailable, i.e. the member it produces will be a MISSING
    /// one (no loaded font, and none installed to import).
    fn is_missing(&self) -> bool {
        match self {
            Self::Missing | Self::InvalidName => true,
            Self::Loaded | Self::SystemImport(_) => false,
        }
    }
}

/// One font-card entry after the worker resolved its PSD-recorded name into a font IDENTITY.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCardEntry {
    /// The identity to store as the group member: the loaded font's identity (`%hash` suffix
    /// included, when it carries one), the identity the located system font declares, or — when
    /// nothing resolved — the card's name VERBATIM, which is exactly what makes the member a
    /// missing one instead of a silently dropped line.
    identity: String,
    /// The layer's text: the member's per-group alias.
    title: String,
    /// How `identity` was obtained; drives the auto-import and the report counts.
    source: CardEntrySource,
}

/// Result of one font-card import attempt, produced entirely on the worker thread.
#[derive(Debug)]
enum CardImportOutcome {
    /// The user closed the file picker without choosing a file. Reported nowhere.
    Cancelled,
    /// The card could not be read or parsed.
    Failed(FontCardError),
    /// The card was read: its entries, deduplicated and resolved, plus how many repeated
    /// entries the deduplication dropped.
    Loaded {
        entries: Vec<ResolvedCardEntry>,
        duplicates: usize,
    },
}

/// What one font-card import did, in the terms the status line reports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CardImportReport {
    /// Members the store really appended to the group.
    added: usize,
    /// Fonts the import installed into the program's font base from the system.
    auto_imported: usize,
    /// NEW members whose font is available nowhere (`invalid` included).
    missing: usize,
    /// New members whose card name is not a valid PostScript name at all — a subset of
    /// `missing`, reported separately because the remedy is different.
    invalid: usize,
    /// Card entries that changed nothing: repeats inside the card plus fonts already in the
    /// group.
    skipped: usize,
}

/// Counts what an applied font card did, from the resolved entries and the store's answer.
///
/// `members` is the group's member list BEFORE the batch, so "already in the group" is decided
/// against the store rather than against anything the worker observed; identities are compared
/// the way the store compares them (trimmed, ASCII-case-insensitive). `duplicates` is what the
/// card-level deduplication dropped, `added` is what `add_virtual_group_members` reported, and
/// `auto_imported` what `add_imported_fonts` reported.
///
/// `missing` counts only entries that were NOT already members: an existing member keeps its
/// row (and its own missing badge) and was not touched by this import.
fn summarize_card_import(
    entries: &[ResolvedCardEntry],
    members: &[VirtualFontGroupMemberInfo],
    duplicates: usize,
    added: usize,
    auto_imported: usize,
) -> CardImportReport {
    let is_member = |identity: &str| {
        members
            .iter()
            .any(|member| identities_equal(&member.font, identity))
    };
    let missing = entries
        .iter()
        .filter(|entry| !is_member(&entry.identity) && entry.source.is_missing())
        .count();
    let invalid = entries
        .iter()
        .filter(|entry| {
            !is_member(&entry.identity) && entry.source == CardEntrySource::InvalidName
        })
        .count();
    CardImportReport {
        added,
        auto_imported,
        missing,
        invalid,
        // Everything the card named that did not become a new member: the repeats the
        // deduplication removed plus the entries the store refused as already present.
        skipped: duplicates + entries.len().saturating_sub(added),
    }
}

/// Renders a [`CardImportReport`] as the localized status line, appending the invalid-name note
/// only when there is something to note.
fn card_import_report_line(report: &CardImportReport) -> String {
    let mut line = tf!(
        "typing.font_settings.group_card_import_report",
        added = report.added,
        imported = report.auto_imported,
        missing = report.missing,
        skipped = report.skipped
    );
    if report.invalid > 0 {
        line.push(' ');
        line.push_str(&tf!(
            "typing.font_settings.group_card_import_invalid_note",
            count = report.invalid
        ));
    }
    line
}

/// Whether a virtual group named exactly `name` is still in the STORE.
///
/// Asked at COMMIT time, not read off `FontGroupsEditorState::groups`: that cache is refreshed
/// once per frame from the store revision, and the deletion this guard exists for can land in
/// the very frame the import result arrives (the group list, which owns the delete button, is
/// drawn before the editor window). Cheap — an in-memory snapshot of the group list — and
/// called once per finished import, never per frame.
fn virtual_group_exists(name: &str) -> bool {
    font_admin::list_virtual_groups()
        .iter()
        .any(|group| group.name == name)
}

/// Whether two font identities denote the same font.
///
/// Delegates to [`font_admin::normalize_font_identity`], the app-wide identity rule, rather
/// than re-implementing "trimmed, case-insensitive" here: the store compares members that way
/// and the typing panel resolves them that way, so the UI's counts match what the store
/// actually did only as long as all three use the one definition.
fn identities_equal(a: &str, b: &str) -> bool {
    font_admin::normalize_font_identity(a) == font_admin::normalize_font_identity(b)
}

/// Drops repeated fonts inside one card, KEEPING the first title each font was given.
///
/// A card may legitimately name one font twice (two sample lines, a heading plus a specimen).
/// The group can only hold the font once, and the first spelling is the one the user wrote
/// first, so later repeats are dropped and counted for the report rather than overwriting it.
/// Fonts are compared like identities are, through [`font_admin::normalize_font_identity`].
fn dedup_card_entries(entries: Vec<FontCardEntry>) -> (Vec<FontCardEntry>, usize) {
    let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
    let mut kept: Vec<FontCardEntry> = Vec::with_capacity(entries.len());
    let mut duplicates = 0usize;
    for entry in entries {
        if seen.insert(font_admin::normalize_font_identity(&entry.post_script_name)) {
            kept.push(entry);
        } else {
            duplicates += 1;
        }
    }
    (kept, duplicates)
}

/// A card name matched against the loaded fonts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CardIdentityMatch {
    /// The loaded font's identity, VERBATIM (it may carry the `%hash` collision suffix, which
    /// the card's bare name never does).
    identity: String,
    /// How many loaded fonts matched. More than one means the name is ambiguous and the first
    /// candidate in list order was taken; the caller logs that.
    candidates: usize,
}

/// Resolves a font name recorded in a card against the identities of the loaded fonts.
///
/// A PSD stores a bare PostScript name, while a loaded font's identity may carry the `%hash`
/// collision suffix the app appends when two byte-different files claim one name. So the match
/// is tried in two passes, both under [`font_admin::normalize_font_identity`]:
/// 1. against the FULL identity — an exact name wins over any suffixed one;
/// 2. against the identity's BASE part (everything before the `%`), which is where a suffixed
///    font's real PostScript name lives.
///
/// Returns `None` for a blank name and when neither pass matches. When a pass matches several
/// fonts, the FIRST in `loaded` order is returned (the categories order: folder fonts before
/// imported ones) and the count comes back with it so the caller can log the ambiguity.
fn resolve_card_identity(name: &str, loaded: &[String]) -> Option<CardIdentityMatch> {
    let needle = font_admin::normalize_font_identity(name);
    if needle.is_empty() {
        return None;
    }
    for use_base in [false, true] {
        let mut matches = loaded.iter().filter(|identity| {
            let candidate = if use_base {
                identity_base(identity)
            } else {
                identity.as_str()
            };
            font_admin::normalize_font_identity(candidate) == needle
        });
        if let Some(first) = matches.next() {
            return Some(CardIdentityMatch {
                identity: first.clone(),
                // `first` is already consumed, so the rest of the iterator is the remainder.
                candidates: 1 + matches.count(),
            });
        }
    }
    None
}

/// The base part of a font identity: everything before the collision suffix, trimmed.
/// An identity without a suffix is its own base.
///
/// The result is still a RAW identity fragment — normalize it before comparing it
/// ([`font_admin::normalize_font_identity`]).
fn identity_base(identity: &str) -> &str {
    match identity.find(IDENTITY_HASH_SEPARATOR) {
        Some(at) => identity[..at].trim(),
        None => identity.trim(),
    }
}

/// Spawns the font-card import worker and returns the receiver of its single result.
///
/// `loaded` is the snapshot of loaded font identities the worker resolves card names against.
/// Everything blocking happens on that thread: the native file picker, the file read, the PSD
/// parse and the (potentially very heavy) by-name system-font lookups.
///
/// # Errors
/// Returns the OS error when the thread cannot be started; nothing is then in flight and the
/// caller reports it.
fn spawn_card_import(loaded: Vec<String>) -> std::io::Result<mpsc::Receiver<CardImportOutcome>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("settings-font-card-import".to_string())
        .spawn(move || {
            let outcome = run_card_import(&loaded);
            if tx.send(outcome).is_err() {
                // The editor window was closed while the import ran: the result has nowhere to
                // go, which is expected rather than an error.
                runtime_log::log_warn(
                    "[settings] font-card import finished after its editor window closed; result dropped",
                );
            }
        })?;
    Ok(rx)
}

/// The whole worker-side import: pick a file, read the card, deduplicate it and resolve every
/// name to a font identity. Blocking throughout; returns the one value the GUI polls.
fn run_card_import(loaded: &[String]) -> CardImportOutcome {
    let Some(path) = pick_font_card_path() else {
        return CardImportOutcome::Cancelled;
    };
    let entries = match read_font_card(&path) {
        Ok(entries) => entries,
        Err(error) => return CardImportOutcome::Failed(error),
    };
    let (unique, duplicates) = dedup_card_entries(entries);
    let entries: Vec<ResolvedCardEntry> = unique
        .into_iter()
        .map(|entry| resolve_card_entry(entry, loaded))
        .collect();
    runtime_log::log_info(format!(
        "[settings] font card '{}' read: {} fonts, {duplicates} repeated entries dropped",
        path.display(),
        entries.len()
    ));
    CardImportOutcome::Loaded {
        entries,
        duplicates,
    }
}

/// Resolves ONE card entry into the identity the group will store, consulting the system-font
/// database only when the name matches nothing already loaded. Blocking (see
/// `font_admin::locate_system_font_by_identity`); worker-thread only.
fn resolve_card_entry(entry: FontCardEntry, loaded: &[String]) -> ResolvedCardEntry {
    let FontCardEntry {
        post_script_name,
        title,
    } = entry;
    if let Some(matched) = resolve_card_identity(&post_script_name, loaded) {
        if matched.candidates > 1 {
            // Ambiguity is not an error — the app resolves a bare name the same way everywhere
            // — but it is worth a line, since the card cannot say WHICH claimant it meant.
            runtime_log::log_warn(format!(
                "[settings] font card name '{post_script_name}' matches {} loaded fonts; taking '{}'",
                matched.candidates, matched.identity
            ));
        }
        return ResolvedCardEntry {
            identity: matched.identity,
            title,
            source: CardEntrySource::Loaded,
        };
    }
    // Not loaded. A name that cannot be a PostScript name at all is not worth an OS-wide
    // lookup; it is kept as a missing member so the card's line is not lost silently.
    if !font_admin::is_valid_post_script_name(&post_script_name) {
        runtime_log::log_warn(format!(
            "[settings] font card names '{post_script_name}', which is not a valid PostScript name; added as a missing member"
        ));
        return ResolvedCardEntry {
            identity: post_script_name,
            title,
            source: CardEntrySource::InvalidName,
        };
    }
    match font_admin::locate_system_font_by_identity(&post_script_name) {
        Some(located) => ResolvedCardEntry {
            identity: located.identity,
            title,
            source: CardEntrySource::SystemImport(located.path),
        },
        None => ResolvedCardEntry {
            identity: post_script_name,
            title,
            source: CardEntrySource::Missing,
        },
    }
}

/// Opens the native single-file picker filtered to PSD documents and returns the chosen path,
/// or `None` when the user cancelled. BLOCKING: worker-thread only.
#[cfg(not(target_arch = "wasm32"))]
fn pick_font_card_path() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .add_filter(t!("typing.font_settings.font_card_filter"), &["psd"])
        .pick_file()
}

/// Web stub: the browser build links no native file dialog (`rfd` is a desktop-only
/// dependency), so the pick resolves as cancelled and the dropped capability is logged.
///
/// Unreachable in practice — the import button is disabled on this target
/// ([`CARD_IMPORT_AVAILABLE`]), which is where the user actually learns the feature is
/// unavailable. The stub exists so the pipeline still compiles for the web build, and logs in
/// case a future call site forgets the gate.
#[cfg(target_arch = "wasm32")]
fn pick_font_card_path() -> Option<PathBuf> {
    runtime_log::log_warn("[settings] native file picker unavailable on web build; font-card import skipped");
    None
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
        let fixed = fixed_columns_width(spacing);
        assert!(
            (fixed - (ALIAS_EDIT_WIDTH + MISSING_COL_WIDTH + REMOVE_COL_WIDTH + 4.0 * spacing))
                .abs()
                < f32::EPSILON,
            "the fixed part covers all THREE fixed columns and the four gaps between five columns"
        );
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
        assert!((columns.missing - MISSING_COL_WIDTH).abs() < f32::EPSILON);
        assert!((columns.remove - REMOVE_COL_WIDTH).abs() < f32::EPSILON);
    }

    /// One card entry, as `font_card_psd` produces it.
    fn card_entry(post_script_name: &str, title: &str) -> FontCardEntry {
        FontCardEntry {
            post_script_name: post_script_name.to_string(),
            title: title.to_string(),
        }
    }

    /// One resolved card entry with the given provenance.
    fn resolved(identity: &str, title: &str, source: CardEntrySource) -> ResolvedCardEntry {
        ResolvedCardEntry {
            identity: identity.to_string(),
            title: title.to_string(),
            source,
        }
    }

    #[test]
    fn a_card_name_resolves_to_an_exact_identity_before_a_suffixed_one() {
        let loaded = vec![
            "CCWildWords-Regular%1122334455667788".to_string(),
            "CCWildWords-Regular".to_string(),
            "Roboto-Bold".to_string(),
        ];
        // The bare name a PSD records matches the FULL identity first, even though a suffixed
        // identity would also match on its base part.
        let matched = resolve_card_identity("CCWildWords-Regular", &loaded)
            .expect("an exact identity must resolve");
        assert_eq!(matched.identity, "CCWildWords-Regular");
        assert_eq!(matched.candidates, 1);
        // Case and surrounding whitespace are irrelevant, exactly as in the store.
        assert_eq!(
            resolve_card_identity("  roboto-bold ", &loaded).map(|m| m.identity),
            Some("Roboto-Bold".to_string())
        );
    }

    #[test]
    fn a_card_name_falls_back_to_the_identity_base_before_the_hash_suffix() {
        // The only claimant of this name carries the collision suffix; the card cannot know it,
        // so the base part is what the name is matched against, and the SUFFIXED identity is
        // what the group must store (that is the key the panel resolves).
        let loaded = vec!["Acme%1122334455667788".to_string()];
        let matched =
            resolve_card_identity("Acme", &loaded).expect("the base part must resolve the name");
        assert_eq!(matched.identity, "Acme%1122334455667788");
        assert_eq!(matched.candidates, 1);

        // Two suffixed claimants: the first in list order wins and the ambiguity is reported.
        let contested = vec![
            "Acme%1111111111111111".to_string(),
            "Acme%2222222222222222".to_string(),
        ];
        let matched = resolve_card_identity("Acme", &contested).expect("a contested name resolves");
        assert_eq!(matched.identity, "Acme%1111111111111111");
        assert_eq!(matched.candidates, 2, "the caller is told the name is ambiguous");

        // A miss stays a miss — that is what makes a member "missing" instead of wrong.
        assert_eq!(resolve_card_identity("Nothing-Regular", &contested), None);
        assert_eq!(resolve_card_identity("   ", &contested), None);
    }

    #[test]
    fn a_font_repeated_inside_a_card_keeps_its_first_title() {
        let (kept, duplicates) = dedup_card_entries(vec![
            card_entry("A-Regular", "Первый"),
            card_entry("B-Bold", "Жирный"),
            // Same font, different spelling of the name and a different title: dropped.
            card_entry("a-regular", "Второй"),
            card_entry(" A-Regular ", "Третий"),
        ]);
        assert_eq!(duplicates, 2, "both repeats are counted for the report");
        let kept: Vec<(String, String)> = kept
            .into_iter()
            .map(|entry| (entry.post_script_name, entry.title))
            .collect();
        assert_eq!(
            kept,
            vec![
                ("A-Regular".to_string(), "Первый".to_string()),
                ("B-Bold".to_string(), "Жирный".to_string()),
            ],
            "the FIRST title wins and the card's order is preserved"
        );
    }

    #[test]
    fn the_status_line_counts_what_the_card_actually_did() {
        let entries = vec![
            resolved("A-Regular", "Обычный", CardEntrySource::Loaded),
            resolved(
                "B-Bold",
                "Жирный",
                CardEntrySource::SystemImport(PathBuf::from("B-Bold.ttf")),
            ),
            resolved("C-Italic", "Курсив", CardEntrySource::Missing),
            resolved("D Broken", "Сломанный", CardEntrySource::InvalidName),
            // Already in the group: the store skips it, and it is NOT counted as missing even
            // though its font is not loaded — this import did not touch that row.
            resolved("E-Old", "Старый", CardEntrySource::Missing),
        ];
        // The store answered: four of the five were appended, "E-Old" was already there.
        let report = summarize_card_import(&entries, &[member("e-old", None)], 1, 4, 1);
        assert_eq!(
            report,
            CardImportReport {
                added: 4,
                auto_imported: 1,
                missing: 2,
                invalid: 1,
                // one card repeat + the one entry the store refused
                skipped: 2,
            }
        );

        let line = card_import_report_line(&report);
        assert!(
            line.contains(&tf!(
                "typing.font_settings.group_card_import_invalid_note",
                count = report.invalid
            )),
            "the invalid-name note is appended when there is something to note: {line}"
        );
        let clean = CardImportReport {
            added: 2,
            auto_imported: 0,
            missing: 0,
            invalid: 0,
            skipped: 0,
        };
        assert_eq!(
            card_import_report_line(&clean),
            tf!(
                "typing.font_settings.group_card_import_report",
                added = 2,
                imported = 0,
                missing = 0,
                skipped = 0
            ),
            "with no invalid names the line is the report sentence alone"
        );
    }

    /// The loaded-identity set exactly as `FontGroupsEditorState::loaded_identity_set` builds
    /// it: every loaded identity run through the app-wide normalization.
    fn loaded_set(identities: &[&str]) -> HashSet<String> {
        identities
            .iter()
            .map(|name| font_admin::normalize_font_identity(name))
            .collect()
    }

    /// A virtual group named "Экшн" holding `members`.
    fn group_of(members: Vec<VirtualFontGroupMemberInfo>) -> VirtualFontGroupInfo {
        VirtualFontGroupInfo {
            name: "Экшн".to_string(),
            members,
        }
    }

    #[test]
    fn a_group_is_flagged_when_one_of_its_members_is_not_loaded() {
        let loaded = loaded_set(&["A-Regular", "Acme%1122334455667788"]);
        assert!(
            !group_has_missing_font(&group_of(Vec::new()), &loaded),
            "an empty group is fine"
        );
        assert!(!group_has_missing_font(
            &group_of(vec![
                member("A-Regular", None),
                member("Acme%1122334455667788", Some("Акме")),
            ]),
            &loaded
        ));
        assert!(
            group_has_missing_font(
                &group_of(vec![member("A-Regular", None), member("Gone", None)]),
                &loaded
            ),
            "one unloaded member is enough to flag the whole group"
        );
        // The collision SUFFIX is part of the identity: the bare base of a suffixed identity is
        // a different member, and normalization (which only folds case and whitespace) does not
        // blur that.
        assert!(group_has_missing_font(
            &group_of(vec![member("Acme", None)]),
            &loaded
        ));
    }

    #[test]
    fn a_member_stored_in_another_casing_is_not_reported_missing() {
        // A member key the deferred legacy migration deliberately left alone: the document
        // records `roboto-bold` while the loaded font declares `Roboto-Bold`. The typing panel
        // merges the two (it normalizes), so the settings UI must not paint the row red.
        let loaded = loaded_set(&["Roboto-Bold", "Acme%1122334455667788"]);
        for stored in ["roboto-bold", "ROBOTO-BOLD", "  Roboto-Bold  ", "\troboto-BOLD\n"] {
            assert!(
                member_is_available(stored, &loaded),
                "{stored:?} must resolve to the loaded Roboto-Bold"
            );
        }
        // The suffixed identity folds the same way, suffix included.
        assert!(member_is_available(" acme%1122334455667788 ", &loaded));
        // A genuinely absent font is still absent.
        assert!(!member_is_available("Roboto-Regular", &loaded));
        assert!(!member_is_available("", &loaded));
        // The group flag reads from exactly the same rule.
        assert!(!group_has_missing_font(
            &group_of(vec![member("roboto-bold", Some("Жирный"))]),
            &loaded
        ));
        assert!(group_has_missing_font(
            &group_of(vec![member("Roboto-Regular", None)]),
            &loaded
        ));
    }

    #[test]
    fn the_bundled_interface_font_is_never_reported_missing() {
        // The synthetic bundled entry is in NEITHER font category, so it can never be in the
        // loaded set — but the panel prepends it to its own list, so a member holding its
        // identity IS shown there. The literals are the persisted contract (current spelling
        // plus the read-only legacy alias); if they ever change, this test must change with
        // them.
        let loaded = loaded_set(&["Roboto-Bold"]);
        for stored in [
            "ManhwaStudio-UI",
            "manhwastudio-ui",
            "  ManhwaStudio-UI ",
            "ManhwaStudio UI",
        ] {
            assert!(
                member_is_available(stored, &loaded),
                "{stored:?} names the built-in interface font and must not be flagged"
            );
        }
        assert!(!group_has_missing_font(
            &group_of(vec![
                member("ManhwaStudio-UI", None),
                member("Roboto-Bold", None),
            ]),
            &loaded
        ));
        // A name that merely looks similar is not the reserved identity.
        assert!(!member_is_available("ManhwaStudio-UI-Bold", &loaded));
    }

    #[test]
    fn the_bottom_row_stacks_only_when_the_three_buttons_do_not_fit() {
        let spacing = 8.0;
        let imports = 300.0;
        let apply = 100.0;
        // Exactly enough room for both halves and the gap between them: one row.
        let exact = imports + spacing + apply;
        assert!(!bottom_row_is_stacked(imports, apply, spacing, exact));
        assert!(!bottom_row_is_stacked(imports, apply, spacing, exact + 200.0));
        // One point short — `ui.horizontal` does not wrap, so the apply button would be
        // squeezed and its caption clipped. Stack instead.
        assert!(bottom_row_is_stacked(imports, apply, spacing, exact - 1.0));
        // A longer locale widens the captions at an unchanged window width.
        assert!(bottom_row_is_stacked(imports * 2.0, apply, spacing, exact));
        assert!(bottom_row_is_stacked(imports, apply * 3.0, spacing, exact));
        // Degenerate widths must not flip the decision the wrong way.
        assert!(bottom_row_is_stacked(imports, apply, spacing, 0.0));
        assert!(!bottom_row_is_stacked(0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn a_card_import_into_a_group_that_disappeared_reports_an_error() {
        let _lock = font_admin::test_lock();
        font_admin::test_reset();
        // The editor still points at "Экшн", but the store no longer has it: it was deleted
        // while the worker read the card.
        let mut ed = editor("Экшн");
        let entries = vec![resolved("A-Regular", "Обычный", CardEntrySource::Loaded)];
        ed.apply_card_import(
            CardImportOutcome::Loaded {
                entries: entries.clone(),
                duplicates: 0,
            },
            &[],
        );
        match ed.card_import_status.take() {
            Some(CardImportStatus::Error(text)) => assert_eq!(
                text,
                t!("typing.font_settings.group_card_import_group_gone"),
                "a vanished group is an error, not a '0 added' report"
            ),
            other => panic!("expected the group-gone error, got {other:?}"),
        }

        // The very same outcome against a group that still exists is a normal report.
        assert!(font_admin::create_virtual_group("Экшн"));
        ed.apply_card_import(
            CardImportOutcome::Loaded {
                entries,
                duplicates: 0,
            },
            &[],
        );
        match ed.card_import_status.take() {
            Some(CardImportStatus::Report(_)) => {}
            other => panic!("expected the counts report, got {other:?}"),
        }
        assert_eq!(
            font_admin::list_virtual_groups()
                .first()
                .map(|group| group.members.len()),
            Some(1),
            "the surviving group really received the card's member"
        );
        font_admin::test_reset();
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
