/*
File: settings/typesetting/font_settings.rs

Purpose:
Self-contained "Настройки шрифтов" editor widget rendered from the settings "Тайп"
pane. Lists the app's fonts in three categories (folder fonts, imported system fonts,
custom fonts), renders each font's name in its own typeface, and lets the user import
an installed system font or remove a previously imported one.

Main responsibilities:
- load the category lists OFF the GUI thread (folder + imported system fonts) and cache
  them, reloading live when the imported-fonts store revision changes;
- render three collapsing categories, each font row drawn in its own font; the per-category
  row lists are virtualized so only visible rows are read+registered per frame. In the folder
  and imported categories each row is a button that opens the per-font PROPERTIES window
  (`font_properties_window`), which owns display-name editing plus off-thread glyph/kerning
  inspection;
- offer, in the folder and imported categories, a per-list switch selecting WHICH name the
  rows show — the user-facing display label or the font's identity (its PostScript name).
  The choice is an INTERFACE preference persisted per list in `user_config.json`
  (`TextTab.font_list_name_mode_*`), applied live and never used as a key. The same switch
  type serves the group-editor window (`FontListKind::Group`, ONE switch driving both of
  that window's lists), which is why the widget and the name-selection helpers live here;
- provide an in-app searchable picker of ALL installed OS fonts (also loaded off-thread,
  virtualized so only visible rows register into egui, and capped so a full-catalog scroll
  registers at most `PICKER_PREVIEW_FONT_CAP` own-typeface previews) to import a font by
  file path;
- host the "Группы" (virtual font groups) sub-editor as a fourth category, delegating to
  `font_groups::FontGroupsEditorState`. The real folder-group names it needs for create-time
  validation are enumerated in the same off-thread category pass (`FontCategories`).

Key types:
- `FontSettingsEditorState`
- `FontNameDisplayMode` / `FontListKind` / `FontNameDisplayModes` (the per-list name switch)

Key functions:
- `FontSettingsEditorState::new` / `FontSettingsEditorState::ui`
- `draw_name_mode_switch` (the shared switch widget; also used by `font_groups.rs`)
- `font_row_matches` (pure search predicate, unit-tested)
- `clean_font_display_name` (pure display-name cleaner, unit-tested)
- `font_row_name_for_mode` / `unavailable_row_name` (pure name selection, unit-tested)

Notes:
This UI reaches the font MODEL ONLY through `crate::tabs::typing::font_admin` (the loaders,
the imported-fonts store, display-name overrides, and the opaque `FontEntry` type). egui
own-typeface registration reuses the shared `crate::widgets` font-preview helpers. Font
enumeration is HEAVY, so both the category lists and the system-font catalog are built on
background threads and delivered over `mpsc` channels; the GUI only polls. Registering a
font into egui inherently needs its bytes, but READING them does not happen here either:
`widgets::request_font_family` queues the read on its own worker threads and the row draws
in the interface font for those frames.
*/

use crate::tabs::settings::save_font_name_display_mode;
use crate::tabs::typing::font_admin::{self, FontEntry};
use crate::widgets::{
    PreviewFontFamily, combo_font_family_name, is_font_family_bound, request_font_family,
};
use ms_thread as thread;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

/// Upper bound on how many DISTINCT preview fonts the import picker registers into egui per
/// open session. egui's `add_font` is ADD-ONLY (no eviction) and every new font triggers a
/// font-atlas rebuild, so scrolling the whole OS catalog would otherwise register hundreds
/// of fonts (hundreds of MB, never reclaimed). Rows beyond the cap render in the default
/// font; the searched/small case still previews every row in its own typeface.
/// `pub(super)` so the group-editor picker (`font_groups.rs`) shares one cap constant instead
/// of duplicating it — both register into the SAME non-evicting egui atlas.
pub(super) const PICKER_PREVIEW_FONT_CAP: usize = 128;

/// Vertical headroom factor for own-typeface preview rows. Rows are drawn in each font's
/// intrinsic face, whose line height can exceed `body_size`; multiply by this so `show_rows`
/// positions rows without clipping or overlap. `pub(super)` so `font_groups.rs` sizes its
/// own-typeface picker/member rows with the same headroom.
pub(super) const PREVIEW_ROW_HEIGHT_FACTOR: f32 = 1.6;

/// Number of preview rows kept visible in a virtualized category list before it scrolls.
const CATEGORY_VISIBLE_ROWS: f32 = 10.0;

/// Which of a font's two names a settings font-list row shows.
///
/// This is an INTERFACE preference of the settings lists, not a property of any font: it
/// changes what is drawn and nothing else. The displayed name is never a key — resolution
/// always goes through the identity (see `src/tabs/typing/panel/MODULE_README.md`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FontNameDisplayMode {
    /// The user-facing name: the user's display-name override when set, else the file-stem
    /// label (`FontEntry::display_label`). The historical behavior, and the default.
    #[default]
    Custom,
    /// The font's IDENTITY — the representative face's PostScript name
    /// (`FontEntry::render_identity_name`), i.e. the name every persisted document uses for
    /// it. A file that declares no usable PostScript name has no identity of its own and
    /// shows the documented identity FALLBACK (family name, else file stem); that fallback
    /// IS what the app uses for that font, so the row stays truthful.
    Identity,
}

impl FontNameDisplayMode {
    /// Stable lowercase token persisted in `user_config.json`.
    #[must_use]
    pub(crate) fn as_config_str(self) -> &'static str {
        match self {
            FontNameDisplayMode::Custom => "custom",
            FontNameDisplayMode::Identity => "identity",
        }
    }

    /// Parses a persisted token; returns `None` for anything unrecognized so the caller
    /// falls back to the default instead of guessing.
    #[must_use]
    pub(crate) fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim() {
            "custom" => Some(FontNameDisplayMode::Custom),
            "identity" => Some(FontNameDisplayMode::Identity),
            _ => None,
        }
    }
}

/// Which switchable font-name surface a [`FontNameDisplayMode`] belongs to.
///
/// Each surface carries its OWN switch and its own persisted value: the folder list, the
/// imported-system list and the group editor are browsed for different reasons, so one shared
/// toggle would fight the user in whichever surface they did not just touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FontListKind {
    /// "Шрифты из папки fonts" — the fonts discovered in the project `fonts/` folder.
    Folder,
    /// "Добавленные системные шрифты" — the imported system fonts, one row per stored entry.
    Imported,
    /// The ADD-MEMBER picker of the virtual-group editor window. It is the only list there
    /// that still has a name to choose: the window's member TABLE shows the user-facing name
    /// and the identity in adjacent columns, so nothing is hidden from it either way.
    Group,
}

impl FontListKind {
    /// The `TextTab` key under which this surface's name-display mode lives in
    /// `user_config.json`. Stable: it is a persisted key, not a label.
    #[must_use]
    pub(crate) fn config_key(self) -> &'static str {
        match self {
            FontListKind::Folder => "font_list_name_mode_folder",
            FontListKind::Imported => "font_list_name_mode_imported",
            FontListKind::Group => "font_list_name_mode_group",
        }
    }

    /// Stable `id_salt` for this surface's switch, so the localized radio labels never decide
    /// the widget ids.
    #[must_use]
    fn switch_id_salt(self) -> &'static str {
        match self {
            FontListKind::Folder => "font_settings_folder_name_mode",
            FontListKind::Imported => "font_settings_imported_name_mode",
            FontListKind::Group => "font_settings_group_name_mode",
        }
    }

    /// Localized leading label of this surface's switch. The group editor's switch governs
    /// only its ADD list, so it names that list where a category switch just says "the list".
    #[must_use]
    fn switch_label(self) -> &'static str {
        match self {
            FontListKind::Folder | FontListKind::Imported => {
                t!("typing.font_settings.name_mode_label")
            }
            FontListKind::Group => t!("typing.font_settings.name_mode_label_group"),
        }
    }
}

/// The name-display mode of each switchable font surface, as loaded from / written to
/// `user_config.json`. Defaults to [`FontNameDisplayMode::Custom`] everywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FontNameDisplayModes {
    /// Mode of the folder-fonts list.
    pub(crate) folder: FontNameDisplayMode,
    /// Mode of the imported-system-fonts list.
    pub(crate) imported: FontNameDisplayMode,
    /// Mode of the group-editor window (both of its lists).
    pub(crate) group: FontNameDisplayMode,
}

impl FontNameDisplayModes {
    /// The mode currently selected for `list`.
    #[must_use]
    pub(crate) fn get(self, list: FontListKind) -> FontNameDisplayMode {
        match list {
            FontListKind::Folder => self.folder,
            FontListKind::Imported => self.imported,
            FontListKind::Group => self.group,
        }
    }

    /// Mutable access to `list`'s slot, so a switch can write back without a match at the
    /// call site.
    fn slot_mut(&mut self, list: FontListKind) -> &mut FontNameDisplayMode {
        match list {
            FontListKind::Folder => &mut self.folder,
            FontListKind::Imported => &mut self.imported,
            FontListKind::Group => &mut self.group,
        }
    }
}

/// A snapshot of the three font categories, loaded together off the GUI thread.
/// `loaded_revision` is the imported-fonts store revision at load time, used to detect
/// staleness (an add/remove bumps the revision → the widget reloads).
struct FontCategories {
    /// Fonts discovered in the project `fonts/` folder.
    folder: Vec<FontEntry>,
    /// The LOADABLE imported system fonts, as list entries (for the group editor's pickers).
    imported: Vec<FontEntry>,
    /// One row per stored imported system font — including the ones that could not be loaded,
    /// which is what makes an unavailable import visible and removable.
    imported_rows: Vec<font_admin::ImportedFontRow>,
    /// Custom (virtual) fonts. Not supported yet; always empty.
    custom: Vec<FontEntry>,
    /// Real folder-group names under `fonts/groups/`, enumerated in this same off-thread pass
    /// (filesystem I/O). Used only by the "Группы" section to reject name collisions on create.
    folder_group_names: Vec<String>,
    /// Imported-fonts store revision at the moment this snapshot was built.
    loaded_revision: u64,
}

/// Editor widget for the settings "Настройки шрифтов" block. Double-interface pattern:
/// self-contained, owns its background loads, and talks only to the font-admin facade
/// (`crate::tabs::typing::font_admin`) — never to the live typing panel.
#[derive(Default)]
pub(crate) struct FontSettingsEditorState {
    /// Cached category lists; `None` until the first background load completes.
    categories: Option<FontCategories>,
    /// In-flight category load, if any.
    categories_rx: Option<mpsc::Receiver<FontCategories>>,
    /// Whether the system-font import picker window is open.
    picker_open: bool,
    /// Cached whole-OS font catalog for the picker; `None` until loaded (kept after the
    /// picker closes so reopening is instant).
    picker_catalog: Option<Vec<FontEntry>>,
    /// In-flight catalog load, if any.
    picker_catalog_rx: Option<mpsc::Receiver<Vec<FontEntry>>>,
    /// Case-insensitive search filter for the picker.
    picker_search: String,
    /// IDENTITY of the font selected in the picker (survives filtering). The row is
    /// identified by what gets STORED on import, not by its file path.
    picker_selected: Option<String>,
    /// egui family names the picker has previewed in their own typeface this open session.
    /// Bounds one-time `add_font` growth via `PICKER_PREVIEW_FONT_CAP`; cleared on close.
    picker_preview_families: HashSet<String>,
    /// The open per-font properties window, if any (at most one at a time).
    properties: Option<super::font_properties_window::FontPropertiesState>,
    /// The "Группы" (virtual font groups) sub-editor rendered as the fourth category.
    groups_editor: super::font_groups::FontGroupsEditorState,
    /// Which name each switchable list shows. Seeded from `user_config.json` by the owner
    /// (`SettingsTabState::new`) and written back off-thread whenever a switch changes.
    name_modes: FontNameDisplayModes,
    /// Path of `user_config.json`, injected by the owner: the target of the name-mode
    /// writes. Empty only in a `Default`-constructed state (tests), where a save would
    /// fail and be logged rather than touch a real config.
    user_settings_file: PathBuf,
}

// `FontEntry` is not `Debug`, so the buffered lists cannot derive it; report structural
// state instead (mirrors `EffectDefaultsEditorState`).
impl std::fmt::Debug for FontSettingsEditorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontSettingsEditorState")
            .field("categories_loaded", &self.categories.is_some())
            .field("categories_loading", &self.categories_rx.is_some())
            .field("picker_open", &self.picker_open)
            .field("picker_catalog_loaded", &self.picker_catalog.is_some())
            .field("properties_open", &self.properties.is_some())
            .field("groups_editor", &self.groups_editor)
            .field("name_modes", &self.name_modes)
            .finish()
    }
}

impl FontSettingsEditorState {
    /// Creates an editor whose category lists load lazily on the first `ui` call.
    ///
    /// `user_settings_file` is the `user_config.json` this widget writes its per-list
    /// name-display modes back to (off the GUI thread); `name_modes` are those modes as
    /// they were read from that file. The read stays with the owner so this constructor
    /// performs no I/O.
    #[must_use]
    pub fn new(user_settings_file: PathBuf, name_modes: FontNameDisplayModes) -> Self {
        Self {
            user_settings_file,
            name_modes,
            ..Self::default()
        }
    }

    /// Renders the font-settings block: three category headers plus the import picker.
    /// Category lists load off-thread and refresh live when the imported-fonts store
    /// mutates. Never blocks the GUI thread with font enumeration.
    ///
    /// `force_reveal_groups` (set only on a deep-link reveal frame) force-opens the nested
    /// "Группы" block and scrolls it into view. Returns that block's rect (header+body
    /// union) when the categories are loaded, for the caller's reveal highlight; `None`
    /// while the category lists are still loading.
    pub fn ui(&mut self, ui: &mut egui::Ui, force_reveal_groups: bool) -> Option<egui::Rect> {
        self.maybe_start_categories_load();
        self.poll_categories_load(ui.ctx());

        ui.label(
            t!("typing.font_settings.description_hint"),
        );
        ui.add_space(4.0);

        // Move the categories out so the collapsing-header closures can mutate `self`
        // (e.g. open the picker) without aliasing the borrowed lists.
        let categories = self.categories.take();
        let groups_rect = match &categories {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("typing.font_settings.loading_status"));
                });
                None
            }
            Some(cats) => Some(self.draw_categories(ui, cats, force_reveal_groups)),
        };
        self.categories = categories;

        self.draw_import_picker(ui.ctx());
        self.draw_properties_window(ui.ctx());
        groups_rect
    }

    /// Opens the per-font properties window for `font` (replacing any currently-open one).
    fn open_properties(&mut self, font: &FontEntry) {
        self.properties = Some(super::font_properties_window::FontPropertiesState::new(font));
    }

    /// Renders the per-font properties window when open; drops its state once closed.
    fn draw_properties_window(&mut self, ctx: &egui::Context) {
        let Some(mut state) = self.properties.take() else {
            return;
        };
        if super::font_properties_window::show(ctx, &mut state) {
            self.properties = Some(state);
        }
    }

    /// Renders the three category collapsing headers from a loaded snapshot plus the fourth
    /// "Группы" section. A font row is a button; clicking one opens that font's properties
    /// window. `force_reveal_groups` force-opens and scrolls to the groups block on the
    /// deep-link reveal frame; returns that block's rect for the caller's reveal highlight.
    fn draw_categories(
        &mut self,
        ui: &mut egui::Ui,
        cats: &FontCategories,
        force_reveal_groups: bool,
    ) -> egui::Rect {
        // A row click sets this; the properties window is opened after the headers so the
        // header closures never need a mutable borrow of `self.properties`.
        let mut to_open: Option<FontEntry> = None;

        egui::CollapsingHeader::new(tf!("typing.font_settings.folder_fonts_header", cats = cats.folder.len()))
            .id_salt("font_settings_folder")
            .default_open(false)
            .show(ui, |ui| {
                if cats.folder.is_empty() {
                    ui.small(t!("typing.font_settings.folder_empty_hint"));
                } else {
                    // Switch first: it decides what the rows below it say.
                    self.draw_owned_name_mode_switch(ui, FontListKind::Folder);
                    if let Some(font) = Self::draw_font_rows_virtualized(
                        ui,
                        &cats.folder,
                        "font_settings_folder_rows",
                        self.name_modes.folder,
                    ) {
                        to_open = Some(font);
                    }
                }
            });

        egui::CollapsingHeader::new(tf!("typing.font_settings.imported_fonts_header", cats = cats.imported_rows.len()))
        .id_salt("font_settings_imported")
        .default_open(false)
        .show(ui, |ui| {
            if cats.imported_rows.is_empty() {
                ui.small(t!("typing.font_settings.imported_empty_hint"));
            } else {
                // Own switch, independent of the folder list's (see `FontListKind`).
                self.draw_owned_name_mode_switch(ui, FontListKind::Imported);
                let name_mode = self.name_modes.imported;
                // Virtualized like the folder list; each row additionally carries a remove
                // button that drives the store (bumping its revision → the lists reload).
                let row_height =
                    egui::TextStyle::Body.resolve(ui.style()).size * PREVIEW_ROW_HEIGHT_FACTOR;
                egui::ScrollArea::vertical()
                    .id_salt("font_settings_imported_rows")
                    .max_height(row_height * CATEGORY_VISIBLE_ROWS)
                    .auto_shrink([false, true])
                    .show_rows(ui, row_height, cats.imported_rows.len(), |ui, range| {
                        for index in range {
                            let Some(row) = cats.imported_rows.get(index) else {
                                continue;
                            };
                            ui.horizontal(|ui| {
                                if ui
                                    .small_button("✕")
                                    .on_hover_text(t!("typing.font_settings.remove_imported_tooltip"))
                                    .clicked()
                                {
                                    // The STORED identity is the document key; the loaded
                                    // entry's render identity may carry a collision suffix
                                    // and would match nothing.
                                    font_admin::remove_imported_font(&row.stored_identity);
                                }
                                match &row.font {
                                    Some(font) => {
                                        if Self::draw_font_name_row(ui, font, name_mode) {
                                            to_open = Some(font.clone());
                                        }
                                    }
                                    // No file to render it with (and nothing to open): show
                                    // what the document records plus the reason, so the user
                                    // can recognize the entry they are about to remove.
                                    None => {
                                        Self::draw_unavailable_imported_row(ui, row, name_mode);
                                    }
                                }
                            });
                        }
                    });
            }
            ui.add_space(4.0);
            // Kept OUTSIDE the scrolled row area so it stays reachable regardless of scroll.
            if ui.button(t!("typing.font_settings.import_from_system_button")).clicked() {
                self.picker_open = true;
            }
        });

        egui::CollapsingHeader::new(t!("typing.font_settings.custom_fonts_header"))
            .id_salt("font_settings_custom")
            .default_open(false)
            .show(ui, |ui| {
                // `custom` is intentionally empty for now; still read it so the field is
                // wired for the future virtual-font category.
                if cats.custom.is_empty() {
                    ui.small(t!("typing.font_settings.custom_fonts_unsupported_hint"));
                } else if let Some(font) = Self::draw_font_rows_virtualized(
                    ui,
                    &cats.custom,
                    "font_settings_custom_rows",
                    // This category has no switch of its own (it is always empty today);
                    // it draws the historical display label until it gains one.
                    FontNameDisplayMode::Custom,
                ) {
                    to_open = Some(font);
                }
            });

        if let Some(font) = to_open {
            self.open_properties(&font);
        }

        // Fourth category: virtual font groups. It owns its own collapsing header and the
        // floating group-editor window; the folder-group names and the loaded font lists come
        // from this off-thread snapshot (no GUI-thread filesystem work). Returns the block
        // rect (for the reveal highlight) and force-opens/scrolls on the deep-link frame.
        let fonts = super::font_groups::GroupEditorFonts {
            folder_group_names: &cats.folder_group_names,
            folder: &cats.folder,
            imported: &cats.imported,
            categories_revision: cats.loaded_revision,
        };
        // The group editor's switch writes straight into our own mode slot (the widget owns
        // ALL the persisted modes); compare around the call to catch the flip and persist it
        // exactly like the two category switches do.
        let previous_group_mode = self.name_modes.group;
        let groups_rect = self.groups_editor.ui(
            ui,
            &fonts,
            force_reveal_groups,
            &mut self.name_modes.group,
        );
        if self.name_modes.group != previous_group_mode {
            self.persist_name_mode(FontListKind::Group, self.name_modes.group);
        }
        groups_rect
    }

    /// Draws one owned list's "which name to show" switch above its rows, applying a change
    /// to the live list immediately and persisting it off the GUI thread.
    ///
    /// Only for the surfaces whose mode this widget OWNS (folder / imported). The group
    /// editor's switch is drawn by `font_groups.rs` against a borrowed slot of the same
    /// [`FontNameDisplayModes`]; its persistence is triggered in `draw_categories`.
    fn draw_owned_name_mode_switch(&mut self, ui: &mut egui::Ui, list: FontListKind) {
        let mut mode = self.name_modes.get(list);
        if draw_name_mode_switch(ui, list, &mut mode) {
            *self.name_modes.slot_mut(list) = mode;
            self.persist_name_mode(list, mode);
        }
    }

    /// Persists one list's name-display mode to `user_config.json` on a worker thread.
    ///
    /// Best-effort, like the pane's other preference writers: the live UI already shows the
    /// new mode, so a failed write is logged with its path and reason rather than surfaced.
    fn persist_name_mode(&self, list: FontListKind, mode: FontNameDisplayMode) {
        let path = self.user_settings_file.clone();
        if let Err(err) = thread::Builder::new()
            .name("settings-save-font-name-mode".to_string())
            .spawn(move || {
                if let Err(err) = save_font_name_display_mode(&path, list, mode) {
                    crate::runtime_log::log_error(format!(
                        "[settings] failed to persist font-name display mode '{}' for list '{}' \
                         to {}; error={err}",
                        mode.as_config_str(),
                        list.config_key(),
                        path.display()
                    ));
                }
            })
        {
            crate::runtime_log::log_error(format!(
                "[settings] failed to start font-name display mode save thread; error={err}"
            ));
        }
    }

    /// Draws the body of an imported-font row whose file could not be used this run.
    ///
    /// It shows the identity the DOCUMENT records — the only thing that still identifies the
    /// font — greyed, plus a short reason, with the recorded path and the technical detail in
    /// the hover text. The row has no properties window (there is no file to inspect) but it
    /// keeps its remove button, which is the whole reason it is drawn at all.
    ///
    /// `mode` selects the same two names the loadable rows offer, as far as an unloadable
    /// entry can: there is no `FontEntry` and therefore no display label, so
    /// [`FontNameDisplayMode::Custom`] shows the user's display-name override for the stored
    /// identity when one exists (a font renamed by the user stays recognizable after its file
    /// disappears) and falls back to that identity otherwise, which is exactly what
    /// [`FontNameDisplayMode::Identity`] always shows.
    fn draw_unavailable_imported_row(
        ui: &mut egui::Ui,
        row: &font_admin::ImportedFontRow,
        mode: FontNameDisplayMode,
    ) {
        // The override lookup is an in-memory store read (no I/O) and only runs for the
        // handful of unavailable rows the virtualizer actually draws.
        let display_override = match mode {
            FontNameDisplayMode::Custom => {
                font_admin::display_name_override(row.stored_identity.trim())
            }
            FontNameDisplayMode::Identity => None,
        };
        let name = match unavailable_row_name(mode, &row.stored_identity, display_override.as_deref())
        {
            Some(name) => name,
            // A legacy entry whose name was never learned: the path is all we have.
            None => row
                .last_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| t!("typing.font_settings.imported_unknown_font").to_string()),
        };
        let (reason, detail) = match &row.unavailable {
            Some(font_admin::ImportedFontUnavailability::NoPathHint) => (
                t!("typing.font_settings.imported_unavailable_no_path").to_string(),
                String::new(),
            ),
            Some(font_admin::ImportedFontUnavailability::Unreadable(error)) => (
                t!("typing.font_settings.imported_unavailable_unreadable").to_string(),
                error.clone(),
            ),
            Some(font_admin::ImportedFontUnavailability::Unparsable) => (
                t!("typing.font_settings.imported_unavailable_unparsable").to_string(),
                String::new(),
            ),
            Some(font_admin::ImportedFontUnavailability::NameMismatch { found }) => (
                tf!("typing.font_settings.imported_unavailable_replaced", found = found),
                String::new(),
            ),
            // Unreachable by construction (`font` is `None` exactly when `unavailable` is
            // `Some`), but a row must still say something rather than render blank.
            None => (
                t!("typing.font_settings.imported_unavailable_unreadable").to_string(),
                String::new(),
            ),
        };
        let mut hover = row
            .last_path
            .as_ref()
            .map(|path| tf!("typing.font_settings.properties_file", file = path.display()))
            .unwrap_or_default();
        if !detail.is_empty() {
            if !hover.is_empty() {
                hover.push('\n');
            }
            hover.push_str(&detail);
        }
        let response = ui.weak(clean_font_display_name(&name));
        if hover.is_empty() {
            response.on_hover_text(reason.clone());
        } else {
            response.on_hover_text(format!("{reason}\n{hover}"));
        }
        ui.small(reason);
    }

    /// Draws a virtualized list of own-typeface font-name rows for a category. Only the rows
    /// currently visible are read and registered into egui per frame (egui `add_font` is
    /// non-evicting), so expanding a large `fonts/` folder no longer reads+registers all N
    /// fonts in a single frame. `id_salt` disambiguates sibling scroll areas. Returns the
    /// clicked font (a snapshot clone) when a row was activated, so the caller can open its
    /// properties window. `mode` selects which of the font's names each row shows.
    fn draw_font_rows_virtualized(
        ui: &mut egui::Ui,
        fonts: &[FontEntry],
        id_salt: &str,
        mode: FontNameDisplayMode,
    ) -> Option<FontEntry> {
        let mut clicked: Option<FontEntry> = None;
        let row_height =
            egui::TextStyle::Body.resolve(ui.style()).size * PREVIEW_ROW_HEIGHT_FACTOR;
        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .max_height(row_height * CATEGORY_VISIBLE_ROWS)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, fonts.len(), |ui, range| {
                for row in range {
                    let Some(font) = fonts.get(row) else {
                        continue;
                    };
                    if Self::draw_font_name_row(ui, font, mode) {
                        clicked = Some(font.clone());
                    }
                }
            });
        clicked
    }

    /// Draws one font's name as a frameless BUTTON rendered in its OWN typeface, and
    /// returns whether it was clicked (to open the font's properties window). Asks
    /// `widgets::font_preview` for the font's representative face and always restores the
    /// previous style font override. Draws in the default UI font for the frames the font
    /// file is still being read OFF the GUI thread, and permanently when it cannot be
    /// registered; never panics.
    ///
    /// `mode` selects WHICH name is drawn; the typeface is unaffected, since the preview
    /// registration is keyed by the font's identity and content either way.
    fn draw_font_name_row(ui: &mut egui::Ui, font: &FontEntry, mode: FontNameDisplayMode) -> bool {
        let rep_face = font.representative_face_index();
        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        let prev_override = ui.style().override_font_id.clone();
        let identity = font.render_identity_name();
        // The font is IDENTIFIED by its identity and by the hash of its bytes (which
        // expires the registration when the file behind that identity is replaced); the
        // path is only the byte source.
        if let PreviewFontFamily::Ready(family) = request_font_family(
            ui.ctx(),
            &identity,
            font.content_hash(),
            font.path(),
            rep_face,
        ) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(body_size, family));
        }
        // Either name is DISPLAY only: `display_label()` applies the user display-name
        // override, the identity is the persisted PostScript name. Neither is a render key
        // here. A frameless button reads like a clickable name row while still being a
        // proper interactive widget.
        let clicked = ui
            .add(
                egui::Button::new(font_row_name_for_mode(mode, font.display_label(), &identity))
                    .frame(false),
            )
            .on_hover_text(t!("typing.font_settings.open_properties_tooltip"))
            .clicked();
        ui.style_mut().override_font_id = prev_override;
        clicked
    }

    /// Starts a background category load if none is cached/in-flight OR the cached snapshot
    /// is stale (its `loaded_revision` differs from the current store revision). Reads the
    /// store revision on the GUI thread (cheap) and does the heavy folder scan + entry
    /// building on a worker thread.
    fn maybe_start_categories_load(&mut self) {
        if self.categories_rx.is_some() {
            return;
        }
        let current_revision = font_admin::fonts_revision();
        let stale = self
            .categories
            .as_ref()
            .is_none_or(|cats| cats.loaded_revision != current_revision);
        if !stale {
            return;
        }

        let (tx, rx) = mpsc::channel();
        match thread::Builder::new()
            .name("settings-load-font-categories".to_string())
            .spawn(move || {
                // ONE combined pass: the folder and imported categories must carry the same
                // identities the typing panel resolves, which only a merged list can assign.
                let lists = font_admin::load_font_lists();
                // Enumerated here (filesystem I/O) so the GUI thread never scans the groups dir.
                let folder_group_names = font_admin::list_folder_group_names();
                let snapshot = FontCategories {
                    folder: lists.folder,
                    imported: lists.imported,
                    imported_rows: lists.imported_rows,
                    custom: Vec::new(),
                    folder_group_names,
                    loaded_revision: current_revision,
                };
                // A disconnected receiver only means the widget was dropped; ignore.
                let _ = tx.send(snapshot);
            }) {
            Ok(_handle) => self.categories_rx = Some(rx),
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[settings] failed to start font-categories load thread; error={err}"
                ));
                // Stop retrying every frame until the store revision changes: cache an
                // empty snapshot at the current revision as a best-effort fallback. Retain the
                // previously loaded folder-group names, though: an empty list would weaken the
                // group create/rename collision validation (a virtual group could then be named
                // to match a real folder group, which the panel later silently drops).
                let folder_group_names = self
                    .categories
                    .as_ref()
                    .map(|cats| cats.folder_group_names.clone())
                    .unwrap_or_default();
                self.categories = Some(FontCategories {
                    folder: Vec::new(),
                    imported: Vec::new(),
                    imported_rows: Vec::new(),
                    custom: Vec::new(),
                    folder_group_names,
                    loaded_revision: current_revision,
                });
            }
        }
    }

    /// Polls the in-flight category load; caches the result when ready and keeps the frame
    /// loop alive (`request_repaint`) while loading.
    fn poll_categories_load(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.categories_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(snapshot) => {
                self.categories = Some(snapshot);
                self.categories_rx = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint(),
            Err(TryRecvError::Disconnected) => {
                self.categories_rx = None;
                crate::runtime_log::log_error(
                    "[settings] font-categories load thread ended without sending a result",
                );
            }
        }
    }

    /// Renders the system-font import picker window when open. Non-blocking: the whole-OS
    /// catalog loads on a worker thread; results are virtualized so only visible rows are
    /// built and registered into egui.
    fn draw_import_picker(&mut self, ctx: &egui::Context) {
        if !self.picker_open {
            return;
        }
        self.maybe_start_picker_load();
        self.poll_picker_load(ctx);

        // Take state out so the window closure never aliases `self` (it only touches the
        // locals below); restore afterward.
        let catalog = self.picker_catalog.take();
        let mut selected = self.picker_selected.take();
        let mut search = std::mem::take(&mut self.picker_search);
        let mut preview_families = std::mem::take(&mut self.picker_preview_families);
        let mut window_open = true;
        let mut close_requested = false;
        // `(identity, path)`: importing is the one place a path is still an input — it is
        // the byte-source hint stored beside the name.
        let mut to_add: Option<(String, PathBuf)> = None;

        egui::Window::new(t!("typing.font_settings.import_window_title")).id(egui::Id::new("typing.font_settings.import_window_title"))
            .open(&mut window_open)
            .collapsible(false)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| match &catalog {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("typing.font_settings.system_list_loading_status"));
                    });
                    ui.ctx().request_repaint();
                }
                Some(fonts) => {
                    draw_picker_body(
                        ui,
                        fonts,
                        &mut search,
                        &mut selected,
                        &mut preview_families,
                        &mut to_add,
                        &mut close_requested,
                    );
                }
            });

        // Always keep the catalog cached so reopening the picker is instant.
        self.picker_catalog = catalog;

        if let Some((identity, path)) = to_add
            && !font_admin::add_imported_font(&identity, path.clone())
        {
            crate::runtime_log::log_info(format!(
                "[settings] system font '{identity}' already imported, skipping: {}",
                path.display()
            ));
        }

        // A successful add sets `close_requested`, so the close branch also covers apply.
        if close_requested || !window_open {
            // Reset per-open state so a reopen starts clean (and re-previews fonts within the
            // cap again); the OS catalog stays cached above. Without this, a plain window-X
            // close would leave stale search text / selection behind.
            self.picker_open = false;
            self.picker_search.clear();
            self.picker_selected = None;
            self.picker_preview_families.clear();
        } else {
            self.picker_selected = selected;
            self.picker_search = search;
            self.picker_preview_families = preview_families;
        }
    }

    /// Starts the whole-OS font catalog load if it is neither cached nor in flight.
    fn maybe_start_picker_load(&mut self) {
        if self.picker_catalog.is_some() || self.picker_catalog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        match thread::Builder::new()
            .name("settings-load-system-fonts".to_string())
            .spawn(move || {
                let _ = tx.send(font_admin::load_system_catalog());
            }) {
            Ok(_handle) => self.picker_catalog_rx = Some(rx),
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[settings] failed to start system-fonts load thread; error={err}"
                ));
                // Cache an empty catalog so the picker shows "no fonts" instead of spinning
                // forever and retrying every frame.
                self.picker_catalog = Some(Vec::new());
            }
        }
    }

    /// Polls the in-flight catalog load; caches it when ready and repaints while loading.
    fn poll_picker_load(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.picker_catalog_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(catalog) => {
                self.picker_catalog = Some(catalog);
                self.picker_catalog_rx = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint(),
            Err(TryRecvError::Disconnected) => {
                self.picker_catalog_rx = None;
                self.picker_catalog = Some(Vec::new());
                crate::runtime_log::log_error(
                    "[settings] system-fonts load thread ended without sending a result",
                );
            }
        }
    }
}

/// Renders the loaded picker body: search box, virtualized result rows, and action
/// buttons. Sets `to_add`/`close_requested` for the caller to act on after the window.
/// `preview_families` tracks and BOUNDS the own-typeface previews registered into egui this
/// session (see `PICKER_PREVIEW_FONT_CAP`).
fn draw_picker_body(
    ui: &mut egui::Ui,
    fonts: &[FontEntry],
    search: &mut String,
    selected: &mut Option<String>,
    preview_families: &mut HashSet<String>,
    to_add: &mut Option<(String, PathBuf)>,
    close_requested: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.label(t!("typing.font_settings.search_label"));
        ui.add(
            egui::TextEdit::singleline(search)
                .desired_width(300.0)
                .hint_text(t!("typing.font_settings.search_placeholder")),
        );
    });
    ui.add_space(4.0);

    // Filter once per frame; only the indices survive so virtualization can index back.
    let filtered: Vec<usize> = fonts
        .iter()
        .enumerate()
        .filter(|(_, font)| {
            font_row_matches(
                font.label(),
                font.original_name(),
                font.display_label(),
                &font.render_identity_name(),
                search,
            )
        })
        .map(|(idx, _)| idx)
        .collect();

    if filtered.is_empty() {
        ui.small(t!("typing.font_settings.nothing_found_status"));
    } else {
        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        // Rows are drawn in each font's own face, whose intrinsic line height can exceed
        // `body_size`; give generous headroom so virtualization positions rows without
        // clipping or overlap.
        let row_height = body_size * PREVIEW_ROW_HEIGHT_FACTOR;
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, filtered.len(), |ui, range| {
                for row in range {
                    let Some(&font_idx) = filtered.get(row) else {
                        continue;
                    };
                    let font = &fonts[font_idx];
                    let identity = font.render_identity_name();
                    let is_selected = selected.as_deref() == Some(identity.as_str());
                    let rep_face = font.representative_face_index();
                    // Preview this row in its own typeface only if the family is already bound,
                    // already previewed this session, or we are still under the cap. Beyond the
                    // cap we render in the default font: egui `add_font` never evicts, so an
                    // unbounded catalog scroll would otherwise leak hundreds of MB of atlases.
                    // Most catalog entries carry the `0` "content unknown" hash (the picker
                    // enumerates faces through `fontdb` without reading whole files), so their
                    // key degenerates to `(identity, face)` — the documented sentinel behavior
                    // of `combo_font_family_name`. `fonts::load_system_fonts` resolves a REAL
                    // hash for the few identities two of its files contest, which is what keeps
                    // those two rows from sharing one registration.
                    let content_hash = font.content_hash();
                    let font_name = combo_font_family_name(&identity, content_hash, rep_face);
                    let allow_own = is_font_family_bound(
                        ui.ctx(),
                        &egui::FontFamily::Name(font_name.clone().into()),
                    ) || preview_families.contains(&font_name)
                        || preview_families.len() < PICKER_PREVIEW_FONT_CAP;
                    let prev_override = ui.style().override_font_id.clone();
                    if allow_own
                        && let PreviewFontFamily::Ready(family) = request_font_family(
                            ui.ctx(),
                            &identity,
                            content_hash,
                            font.path(),
                            rep_face,
                        )
                    {
                        ui.style_mut().override_font_id =
                            Some(egui::FontId::new(body_size, family));
                        preview_families.insert(font_name);
                    }
                    let response =
                        ui.selectable_label(is_selected, clean_font_display_name(font.display_label()));
                    ui.style_mut().override_font_id = prev_override;
                    if response.clicked() {
                        *selected = Some(identity);
                    }
                }
            });
    }

    ui.separator();
    let already_imported = selected
        .as_deref()
        .is_some_and(font_admin::is_font_imported);
    ui.horizontal(|ui| {
        let can_add = selected.is_some() && !already_imported;
        if ui
            .add_enabled(can_add, egui::Button::new(t!("typing.font_settings.add_button")))
            .clicked()
        {
            // The selection is an IDENTITY; the catalog row it came from supplies the file
            // path stored beside it as the byte-source hint.
            *to_add = selected.as_ref().and_then(|identity| {
                fonts
                    .iter()
                    .find(|font| font.render_identity_name() == *identity)
                    .map(|font| (identity.clone(), font.path().to_path_buf()))
            });
            *close_requested = true;
        }
        if ui.button(t!("typing.common.cancel_button")).clicked() {
            *close_requested = true;
        }
        if already_imported {
            ui.label(t!("typing.font_settings.already_added_status"));
        }
    });
}

/// Draws a "which name to show" switch (two radio buttons) for `list`, writing the choice
/// through `mode`. Returns `true` when this frame's interaction CHANGED it, so the caller can
/// persist the new value.
///
/// Shared by every switchable surface (`font_settings.rs`'s two category lists and
/// `font_groups.rs`'s group-editor window) so the modes, the labels and the hover texts can
/// never drift apart. The radios carry a stable `id_salt` derived from `list` so their
/// localized labels never decide the widget ids, and so two switches on screen at once keep
/// separate widget state.
pub(super) fn draw_name_mode_switch(
    ui: &mut egui::Ui,
    list: FontListKind,
    mode: &mut FontNameDisplayMode,
) -> bool {
    let previous = *mode;
    ui.push_id(list.switch_id_salt(), |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(list.switch_label());
            ui.radio_value(
                mode,
                FontNameDisplayMode::Custom,
                t!("typing.font_settings.name_mode_custom"),
            )
            .on_hover_text(t!("typing.font_settings.name_mode_custom_hint"));
            ui.radio_value(
                mode,
                FontNameDisplayMode::Identity,
                t!("typing.font_settings.name_mode_identity"),
            )
            .on_hover_text(t!("typing.font_settings.name_mode_identity_hint"));
        });
    });
    *mode != previous
}

/// Case-insensitive substring match of a font row against a search query. Empty/whitespace
/// query matches everything. Matches the render `label`, the `original_name`, the
/// `display_label` OR the `identity` — a picker row can SHOW any of them (the display label is
/// a user rename override; the identity is what a list switched to
/// [`FontNameDisplayMode::Identity`] draws), so a font must stay findable by every name it can
/// be presented under, not only by its underlying render key.
///
/// Deliberately mode-INDEPENDENT: the search box is not retyped when the name switch flips, so
/// a predicate that narrowed with the mode would silently drop rows the user had already
/// found.
pub(super) fn font_row_matches(
    label: &str,
    original_name: &str,
    display_label: &str,
    identity: &str,
    query: &str,
) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    label.to_lowercase().contains(&needle)
        || original_name.to_lowercase().contains(&needle)
        || display_label.to_lowercase().contains(&needle)
        || identity.to_lowercase().contains(&needle)
}

/// Picks the text a font row shows for `mode`.
///
/// `display_label` is the presentation name (user override → file-stem label) and `identity`
/// is the font's render identity — its PostScript name, or the documented family/file-stem
/// FALLBACK when the file declares no spec-valid one. A blank `identity` (which no loaded
/// entry produces, but which costs nothing to guard) falls back to the display label, so a
/// row is never drawn empty.
pub(super) fn font_row_name_for_mode(
    mode: FontNameDisplayMode,
    display_label: &str,
    identity: &str,
) -> String {
    match mode {
        FontNameDisplayMode::Custom => clean_font_display_name(display_label),
        FontNameDisplayMode::Identity => {
            let identity = identity.trim();
            if identity.is_empty() {
                clean_font_display_name(display_label)
            } else {
                identity.to_string()
            }
        }
    }
}

/// Picks the text a row with NO loaded `FontEntry` shows — an unavailable imported font, or a
/// group member whose font is not currently loaded: only the identity the document stores plus
/// (in [`FontNameDisplayMode::Custom`]) the user's display-name override for it.
///
/// Returns `None` when the document records no usable identity — a legacy entry whose name was
/// never learned — so the caller can fall back to the recorded path or a localized placeholder.
/// A blank override is treated as absent.
pub(super) fn unavailable_row_name(
    mode: FontNameDisplayMode,
    stored_identity: &str,
    display_override: Option<&str>,
) -> Option<String> {
    let stored = stored_identity.trim();
    if stored.is_empty() {
        return None;
    }
    match mode {
        FontNameDisplayMode::Custom => Some(
            display_override
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(stored)
                .to_string(),
        ),
        FontNameDisplayMode::Identity => Some(stored.to_string()),
    }
}

/// Strips the internal `" [system]"` marker from a font label for display. The `" (N)"`
/// duplicate-disambiguation suffix (if any) is preserved so distinct files stay distinct.
pub(super) fn clean_font_display_name(label: &str) -> String {
    label.replace(" [system]", "")
}

#[cfg(test)]
mod tests {
    use super::{
        FontListKind, FontNameDisplayMode, FontNameDisplayModes, clean_font_display_name,
        font_row_matches, font_row_name_for_mode, unavailable_row_name,
    };

    #[test]
    fn empty_query_matches_everything() {
        assert!(font_row_matches("Arial", "Arial", "Arial", "Arial", ""));
        assert!(font_row_matches("Arial", "Arial", "Arial", "Arial", "   "));
    }

    #[test]
    fn query_matches_label_case_insensitively() {
        assert!(font_row_matches(
            "Roboto-Bold [system]",
            "Roboto",
            "Roboto-Bold",
            "Roboto-Bold",
            "roboto"
        ));
        assert!(font_row_matches(
            "Roboto-Bold [system]",
            "Roboto",
            "Roboto-Bold",
            "Roboto-Bold",
            "BOLD"
        ));
    }

    #[test]
    fn query_matches_original_name_when_label_differs() {
        // Label is a file stem; the real family name only lives in `original_name`.
        assert!(font_row_matches(
            "DejaVuSans",
            "DejaVu Sans",
            "DejaVuSans",
            "DejaVuSans",
            "dejavu sans"
        ));
    }

    #[test]
    fn query_matches_display_label_override() {
        // Neither the render label nor the original name contains the needle, but the user's
        // display-name override (what the row SHOWS) does — the row must stay findable.
        assert!(font_row_matches("Roboto-Bold", "Roboto", "Мой шрифт", "Roboto-Bold", "шрифт"));
        assert!(!font_row_matches("Roboto-Bold", "Roboto", "Мой шрифт", "Roboto-Bold", "arial"));
    }

    #[test]
    fn query_matches_the_identity_a_switched_list_shows() {
        // A renamed font in `Identity` mode SHOWS its PostScript name, which matches neither
        // the file stem, the family, nor the rename; searching by what is on screen must work.
        assert!(font_row_matches(
            "wildwords",
            "CC Wild Words",
            "Мой шрифт",
            "CCWildWords-Regular",
            "ccwildwords-regular"
        ));
        // The `%hash` collision suffix is part of that shown name too.
        assert!(font_row_matches(
            "roboto",
            "Roboto",
            "Roboto",
            "Roboto%0123456789abcdef",
            "%0123456789ab"
        ));
    }

    #[test]
    fn non_matching_query_is_rejected() {
        assert!(!font_row_matches("Arial", "Arial", "Arial", "Arial", "comic"));
    }

    #[test]
    fn clean_display_name_strips_system_marker_keeps_dedup_suffix() {
        assert_eq!(clean_font_display_name("Roboto [system]"), "Roboto");
        assert_eq!(clean_font_display_name("Roboto [system] (2)"), "Roboto (2)");
        // A plain folder-font label is unchanged.
        assert_eq!(clean_font_display_name("Comic Sans"), "Comic Sans");
    }

    #[test]
    fn name_mode_config_token_roundtrips_both_variants() {
        for mode in [FontNameDisplayMode::Custom, FontNameDisplayMode::Identity] {
            assert_eq!(
                FontNameDisplayMode::from_config_str(mode.as_config_str()),
                Some(mode)
            );
        }
        // Unknown/blank tokens are rejected so the caller keeps its default.
        assert_eq!(FontNameDisplayMode::from_config_str("postscript"), None);
        assert_eq!(FontNameDisplayMode::from_config_str(""), None);
        // The default is the historical behavior.
        assert_eq!(FontNameDisplayMode::default(), FontNameDisplayMode::Custom);
    }

    #[test]
    fn list_kinds_use_distinct_config_keys_and_id_salts() {
        let kinds = [
            FontListKind::Folder,
            FontListKind::Imported,
            FontListKind::Group,
        ];
        // Every pair must differ: a shared config key would make two switches overwrite each
        // other on disk, and a shared id_salt would fuse their egui widget state.
        for (index, list) in kinds.iter().enumerate() {
            for other in &kinds[index + 1..] {
                assert_ne!(list.config_key(), other.config_key(), "{list:?} vs {other:?}");
                assert_ne!(
                    list.switch_id_salt(),
                    other.switch_id_salt(),
                    "{list:?} vs {other:?}"
                );
            }
        }
    }

    #[test]
    fn modes_are_per_list() {
        let mut modes = FontNameDisplayModes::default();
        *modes.slot_mut(FontListKind::Imported) = FontNameDisplayMode::Identity;
        // Switching one surface must not move the others.
        assert_eq!(modes.get(FontListKind::Folder), FontNameDisplayMode::Custom);
        assert_eq!(
            modes.get(FontListKind::Imported),
            FontNameDisplayMode::Identity
        );
        assert_eq!(modes.get(FontListKind::Group), FontNameDisplayMode::Custom);

        // ...and the group-editor window has its own slot, independent of both lists.
        *modes.slot_mut(FontListKind::Group) = FontNameDisplayMode::Identity;
        *modes.slot_mut(FontListKind::Imported) = FontNameDisplayMode::Custom;
        assert_eq!(modes.get(FontListKind::Group), FontNameDisplayMode::Identity);
        assert_eq!(modes.get(FontListKind::Folder), FontNameDisplayMode::Custom);
        assert_eq!(
            modes.get(FontListKind::Imported),
            FontNameDisplayMode::Custom
        );
    }

    #[test]
    fn row_name_follows_the_mode() {
        // Custom mode shows the presentation label (system marker stripped).
        assert_eq!(
            font_row_name_for_mode(
                FontNameDisplayMode::Custom,
                "Roboto-Bold [system]",
                "Roboto-Bold"
            ),
            "Roboto-Bold"
        );
        // A user rename wins in Custom mode and is invisible in Identity mode.
        assert_eq!(
            font_row_name_for_mode(FontNameDisplayMode::Custom, "Мой шрифт", "CCWildWords-Regular"),
            "Мой шрифт"
        );
        assert_eq!(
            font_row_name_for_mode(
                FontNameDisplayMode::Identity,
                "Мой шрифт",
                "CCWildWords-Regular"
            ),
            "CCWildWords-Regular"
        );
        // A contested identity keeps its `%hash` suffix: that IS the font's name here.
        assert_eq!(
            font_row_name_for_mode(
                FontNameDisplayMode::Identity,
                "Roboto",
                "Roboto%0123456789abcdef"
            ),
            "Roboto%0123456789abcdef"
        );
    }

    #[test]
    fn row_name_never_renders_empty_without_an_identity() {
        // Defensive: an entry with no identity at all still shows its label.
        assert_eq!(
            font_row_name_for_mode(FontNameDisplayMode::Identity, "Broken [system]", "   "),
            "Broken"
        );
    }

    #[test]
    fn unavailable_row_shows_the_stored_identity_in_both_modes() {
        assert_eq!(
            unavailable_row_name(FontNameDisplayMode::Identity, "Roboto-Medium", None),
            Some("Roboto-Medium".to_string())
        );
        assert_eq!(
            unavailable_row_name(FontNameDisplayMode::Custom, "Roboto-Medium", None),
            Some("Roboto-Medium".to_string())
        );
        // Custom mode prefers the user's rename, which is how they know this entry.
        assert_eq!(
            unavailable_row_name(FontNameDisplayMode::Custom, "Roboto-Medium", Some("Крик")),
            Some("Крик".to_string())
        );
        // Identity mode ignores the rename even when one is passed.
        assert_eq!(
            unavailable_row_name(FontNameDisplayMode::Identity, "Roboto-Medium", Some("Крик")),
            Some("Roboto-Medium".to_string())
        );
        // A blank override is not a name.
        assert_eq!(
            unavailable_row_name(FontNameDisplayMode::Custom, "Roboto-Medium", Some("  ")),
            Some("Roboto-Medium".to_string())
        );
    }

    #[test]
    fn unavailable_row_without_stored_identity_has_no_name() {
        // A legacy entry whose name was never learned: the caller falls back to the path.
        for mode in [FontNameDisplayMode::Custom, FontNameDisplayMode::Identity] {
            assert_eq!(unavailable_row_name(mode, "   ", None), None);
            assert_eq!(unavailable_row_name(mode, "", Some("Крик")), None);
        }
    }
}
