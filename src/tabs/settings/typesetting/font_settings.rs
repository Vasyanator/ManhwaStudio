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
- provide an in-app searchable picker of ALL installed OS fonts (also loaded off-thread,
  virtualized so only visible rows register into egui, and capped so a full-catalog scroll
  registers at most `PICKER_PREVIEW_FONT_CAP` own-typeface previews) to import a font by
  file path;
- host the "Группы" (virtual font groups) sub-editor as a fourth category, delegating to
  `font_groups::FontGroupsEditorState`. The real folder-group names it needs for create-time
  validation are enumerated in the same off-thread category pass (`FontCategories`).

Key types:
- `FontSettingsEditorState`

Key functions:
- `FontSettingsEditorState::new` / `FontSettingsEditorState::ui`
- `font_row_matches` (pure search predicate, unit-tested)
- `clean_font_display_name` (pure display-name cleaner, unit-tested)

Notes:
This UI reaches the font MODEL ONLY through `crate::tabs::typing::font_admin` (the loaders,
the imported-fonts store, display-name overrides, and the opaque `FontEntry` type). egui
own-typeface registration reuses the shared `crate::widgets` font-preview helpers. Font
enumeration is HEAVY, so both the category lists and the system-font catalog are built on
background threads and delivered over `mpsc` channels; the GUI only polls. Registering a
font into egui inherently needs its bytes, so per-visible-font one-time file reads happen on
the GUI thread; the heavy enumeration never does.
*/

use crate::tabs::typing::font_admin::{self, FontEntry};
use crate::widgets::{combo_font_family_name, ensure_font_family, is_font_family_bound};
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

/// A snapshot of the three font categories, loaded together off the GUI thread.
/// `loaded_revision` is the imported-fonts store revision at load time, used to detect
/// staleness (an add/remove bumps the revision → the widget reloads).
struct FontCategories {
    /// Fonts discovered in the project `fonts/` folder.
    folder: Vec<FontEntry>,
    /// User-imported system fonts (built from the store's file paths).
    imported: Vec<FontEntry>,
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
    /// Selected font FILE path in the picker (survives filtering).
    picker_selected: Option<PathBuf>,
    /// egui family names the picker has previewed in their own typeface this open session.
    /// Bounds one-time `add_font` growth via `PICKER_PREVIEW_FONT_CAP`; cleared on close.
    picker_preview_families: HashSet<String>,
    /// The open per-font properties window, if any (at most one at a time).
    properties: Option<super::font_properties_window::FontPropertiesState>,
    /// The "Группы" (virtual font groups) sub-editor rendered as the fourth category.
    groups_editor: super::font_groups::FontGroupsEditorState,
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
            .finish()
    }
}

impl FontSettingsEditorState {
    /// Creates an uninitialized editor; category lists load lazily on the first `ui` call.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Renders the font-settings block: three category headers plus the import picker.
    /// Category lists load off-thread and refresh live when the imported-fonts store
    /// mutates. Never blocks the GUI thread with font enumeration.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.maybe_start_categories_load();
        self.poll_categories_load(ui.ctx());

        ui.label(
            t!("typing.font_settings.description_hint"),
        );
        ui.add_space(4.0);

        // Move the categories out so the collapsing-header closures can mutate `self`
        // (e.g. open the picker) without aliasing the borrowed lists.
        let categories = self.categories.take();
        match &categories {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("typing.font_settings.loading_status"));
                });
            }
            Some(cats) => self.draw_categories(ui, cats),
        }
        self.categories = categories;

        self.draw_import_picker(ui.ctx());
        self.draw_properties_window(ui.ctx());
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

    /// Renders the three category collapsing headers from a loaded snapshot. A font row is a
    /// button; clicking one opens that font's properties window.
    fn draw_categories(&mut self, ui: &mut egui::Ui, cats: &FontCategories) {
        // A row click sets this; the properties window is opened after the headers so the
        // header closures never need a mutable borrow of `self.properties`.
        let mut to_open: Option<FontEntry> = None;

        egui::CollapsingHeader::new(tf!("typing.font_settings.folder_fonts_header", cats = cats.folder.len()))
            .id_salt("font_settings_folder")
            .default_open(false)
            .show(ui, |ui| {
                if cats.folder.is_empty() {
                    ui.small(t!("typing.font_settings.folder_empty_hint"));
                } else if let Some(font) =
                    Self::draw_font_rows_virtualized(ui, &cats.folder, "font_settings_folder_rows")
                {
                    to_open = Some(font);
                }
            });

        egui::CollapsingHeader::new(tf!("typing.font_settings.imported_fonts_header", cats = cats.imported.len()))
        .id_salt("font_settings_imported")
        .default_open(false)
        .show(ui, |ui| {
            if cats.imported.is_empty() {
                ui.small(t!("typing.font_settings.imported_empty_hint"));
            } else {
                // Virtualized like the folder list; each row additionally carries a remove
                // button that drives the store (bumping its revision → the lists reload).
                let row_height =
                    egui::TextStyle::Body.resolve(ui.style()).size * PREVIEW_ROW_HEIGHT_FACTOR;
                egui::ScrollArea::vertical()
                    .id_salt("font_settings_imported_rows")
                    .max_height(row_height * CATEGORY_VISIBLE_ROWS)
                    .auto_shrink([false, true])
                    .show_rows(ui, row_height, cats.imported.len(), |ui, range| {
                        for row in range {
                            let Some(font) = cats.imported.get(row) else {
                                continue;
                            };
                            ui.horizontal(|ui| {
                                if ui
                                    .small_button("✕")
                                    .on_hover_text(t!("typing.font_settings.remove_imported_tooltip"))
                                    .clicked()
                                {
                                    font_admin::remove_imported_font(font.path());
                                }
                                if Self::draw_font_name_row(ui, font) {
                                    to_open = Some(font.clone());
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
                } else if let Some(font) =
                    Self::draw_font_rows_virtualized(ui, &cats.custom, "font_settings_custom_rows")
                {
                    to_open = Some(font);
                }
            });

        if let Some(font) = to_open {
            self.open_properties(&font);
        }

        // Fourth category: virtual font groups. It owns its own collapsing header and the
        // floating group-editor window; the folder-group names and the loaded font lists come
        // from this off-thread snapshot (no GUI-thread filesystem work).
        self.groups_editor.ui(
            ui,
            &cats.folder_group_names,
            &cats.folder,
            &cats.imported,
            cats.loaded_revision,
        );
    }

    /// Draws a virtualized list of own-typeface font-name rows for a category. Only the rows
    /// currently visible are read and registered into egui per frame (egui `add_font` is
    /// non-evicting), so expanding a large `fonts/` folder no longer reads+registers all N
    /// fonts in a single frame. `id_salt` disambiguates sibling scroll areas. Returns the
    /// clicked font (a snapshot clone) when a row was activated, so the caller can open its
    /// properties window.
    fn draw_font_rows_virtualized(
        ui: &mut egui::Ui,
        fonts: &[FontEntry],
        id_salt: &str,
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
                    if Self::draw_font_name_row(ui, font) {
                        clicked = Some(font.clone());
                    }
                }
            });
        clicked
    }

    /// Draws one font's display name as a frameless BUTTON rendered in its OWN typeface, and
    /// returns whether it was clicked (to open the font's properties window). Registers the
    /// font's representative face into egui on first use (guarded by `is_font_family_bound`)
    /// and always restores the previous style font override. Falls back to the default font
    /// when the file cannot be registered; never panics.
    fn draw_font_name_row(ui: &mut egui::Ui, font: &FontEntry) -> bool {
        let rep_face = font.representative_face_index();
        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        let prev_override = ui.style().override_font_id.clone();
        if let Some(family) = ensure_font_family(ui.ctx(), font.path(), rep_face) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(body_size, family));
        }
        // `display_label()` applies the user display-name override for presentation;
        // the render key (`label`) is untouched. A frameless button reads like a clickable
        // name row while still being a proper interactive widget.
        let clicked = ui
            .add(egui::Button::new(clean_font_display_name(font.display_label())).frame(false))
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
                let folder = font_admin::load_folder_fonts();
                let imported = font_admin::load_imported_fonts();
                // Enumerated here (filesystem I/O) so the GUI thread never scans the groups dir.
                let folder_group_names = font_admin::list_folder_group_names();
                let snapshot = FontCategories {
                    folder,
                    imported,
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
        let mut to_add: Option<PathBuf> = None;

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

        if let Some(path) = to_add
            && !font_admin::add_imported_font(path.clone())
        {
            crate::runtime_log::log_info(format!(
                "[settings] system font already imported, skipping: {}",
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
    selected: &mut Option<PathBuf>,
    preview_families: &mut HashSet<String>,
    to_add: &mut Option<PathBuf>,
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
            font_row_matches(font.label(), font.original_name(), font.display_label(), search)
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
                    let is_selected = selected.as_deref() == Some(font.path());
                    let rep_face = font.representative_face_index();
                    // Preview this row in its own typeface only if the family is already bound,
                    // already previewed this session, or we are still under the cap. Beyond the
                    // cap we render in the default font: egui `add_font` never evicts, so an
                    // unbounded catalog scroll would otherwise leak hundreds of MB of atlases.
                    let font_name = combo_font_family_name(font.path(), rep_face);
                    let allow_own = is_font_family_bound(
                        ui.ctx(),
                        &egui::FontFamily::Name(font_name.clone().into()),
                    ) || preview_families.contains(&font_name)
                        || preview_families.len() < PICKER_PREVIEW_FONT_CAP;
                    let prev_override = ui.style().override_font_id.clone();
                    if allow_own
                        && let Some(family) = ensure_font_family(ui.ctx(), font.path(), rep_face)
                    {
                        ui.style_mut().override_font_id =
                            Some(egui::FontId::new(body_size, family));
                        preview_families.insert(font_name);
                    }
                    let response =
                        ui.selectable_label(is_selected, clean_font_display_name(font.display_label()));
                    ui.style_mut().override_font_id = prev_override;
                    if response.clicked() {
                        *selected = Some(font.path().to_path_buf());
                    }
                }
            });
    }

    ui.separator();
    let already_imported = selected
        .as_ref()
        .is_some_and(|path| font_admin::is_font_imported(path));
    ui.horizontal(|ui| {
        let can_add = selected.is_some() && !already_imported;
        if ui
            .add_enabled(can_add, egui::Button::new(t!("typing.font_settings.add_button")))
            .clicked()
        {
            *to_add = selected.clone();
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

/// Case-insensitive substring match of a font row against a search query. Empty/whitespace
/// query matches everything. Matches the render `label`, the `original_name`, OR the
/// `display_label` — the rows SHOW the display label (a user rename override), so a renamed
/// font must be findable by its shown name, not only by its underlying render key.
pub(super) fn font_row_matches(
    label: &str,
    original_name: &str,
    display_label: &str,
    query: &str,
) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    label.to_lowercase().contains(&needle)
        || original_name.to_lowercase().contains(&needle)
        || display_label.to_lowercase().contains(&needle)
}

/// Strips the internal `" [system]"` marker from a font label for display. The `" (N)"`
/// duplicate-disambiguation suffix (if any) is preserved so distinct files stay distinct.
pub(super) fn clean_font_display_name(label: &str) -> String {
    label.replace(" [system]", "")
}

#[cfg(test)]
mod tests {
    use super::{clean_font_display_name, font_row_matches};

    #[test]
    fn empty_query_matches_everything() {
        assert!(font_row_matches("Arial", "Arial", "Arial", ""));
        assert!(font_row_matches("Arial", "Arial", "Arial", "   "));
    }

    #[test]
    fn query_matches_label_case_insensitively() {
        assert!(font_row_matches("Roboto-Bold [system]", "Roboto", "Roboto-Bold", "roboto"));
        assert!(font_row_matches("Roboto-Bold [system]", "Roboto", "Roboto-Bold", "BOLD"));
    }

    #[test]
    fn query_matches_original_name_when_label_differs() {
        // Label is a file stem; the real family name only lives in `original_name`.
        assert!(font_row_matches("DejaVuSans", "DejaVu Sans", "DejaVuSans", "dejavu sans"));
    }

    #[test]
    fn query_matches_display_label_override() {
        // Neither the render label nor the original name contains the needle, but the user's
        // display-name override (what the row SHOWS) does — the row must stay findable.
        assert!(font_row_matches("Roboto-Bold", "Roboto", "Мой шрифт", "шрифт"));
        assert!(!font_row_matches("Roboto-Bold", "Roboto", "Мой шрифт", "arial"));
    }

    #[test]
    fn non_matching_query_is_rejected() {
        assert!(!font_row_matches("Arial", "Arial", "Arial", "comic"));
    }

    #[test]
    fn clean_display_name_strips_system_marker_keeps_dedup_suffix() {
        assert_eq!(clean_font_display_name("Roboto [system]"), "Roboto");
        assert_eq!(clean_font_display_name("Roboto [system] (2)"), "Roboto (2)");
        // A plain folder-font label is unchanged.
        assert_eq!(clean_font_display_name("Comic Sans"), "Comic Sans");
    }
}
