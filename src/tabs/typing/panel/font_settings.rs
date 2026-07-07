/*
File: panel/font_settings.rs

Purpose:
Self-contained "Настройки шрифтов" editor widget rendered from the settings "Тайп"
pane. Lists the app's fonts in three categories (folder fonts, imported system fonts,
custom fonts), renders each font's name in its own typeface, and lets the user import
an installed system font or remove a previously imported one.

Main responsibilities:
- load the category lists OFF the GUI thread (folder + imported system fonts) and cache
  them, reloading live when the imported-fonts store revision changes;
- render three collapsing categories, each font row drawn in its own font; the per-category
  row lists are virtualized so only visible rows are read+registered per frame;
- provide an in-app searchable picker of ALL installed OS fonts (also loaded off-thread,
  virtualized so only visible rows register into egui, and capped so a full-catalog scroll
  registers at most `PICKER_PREVIEW_FONT_CAP` own-typeface previews) to import a font by
  file path;
- drive the runtime-global imported-fonts store (`font_settings_store`) for add/remove.

Key types:
- `FontSettingsEditorState`

Key functions:
- `FontSettingsEditorState::new` / `FontSettingsEditorState::ui`
- `font_row_matches` (pure search predicate, unit-tested)
- `clean_font_display_name` (pure display-name cleaner, unit-tested)

Notes:
`use super::*;` pulls in the parent `panel` module's types and imports (font loaders
`load_fonts_from_dir` / `load_imported_system_fonts` / `load_system_fonts` /
`resolve_fonts_dir`, the `combo_font_family_name` / `is_font_family_bound` helpers,
`FontEntry`, `thread` = `ms_thread`, `mpsc`, `fs`, `Path`/`PathBuf`, `egui`). The store
mutators live in `super::font_settings_store`. Font enumeration is HEAVY, so both the
category lists and the system-font catalog are built on background threads and delivered
over `mpsc` channels; the GUI only polls. Registering a font into egui inherently needs
its bytes, so per-visible-font one-time file reads happen on the GUI thread exactly like
`create_presets::ensure_combo_font_family`; the heavy enumeration never does.
*/

use super::*;
use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;

/// Upper bound on how many DISTINCT preview fonts the import picker registers into egui per
/// open session. egui's `add_font` is ADD-ONLY (no eviction) and every new font triggers a
/// font-atlas rebuild, so scrolling the whole OS catalog would otherwise register hundreds
/// of fonts (hundreds of MB, never reclaimed). Rows beyond the cap render in the default
/// font; the searched/small case still previews every row in its own typeface.
const PICKER_PREVIEW_FONT_CAP: usize = 128;

/// Vertical headroom factor for own-typeface preview rows. Rows are drawn in each font's
/// intrinsic face, whose line height can exceed `body_size`; multiply by this so `show_rows`
/// positions rows without clipping or overlap.
const PREVIEW_ROW_HEIGHT_FACTOR: f32 = 1.6;

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
    /// Imported-fonts store revision at the moment this snapshot was built.
    loaded_revision: u64,
}

/// Editor widget for the settings "Настройки шрифтов" block. Double-interface pattern:
/// self-contained, owns its background loads, and talks only to runtime globals
/// (`font_settings_store`) and the font loaders — never to the live typing panel.
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
            "Списки шрифтов приложения по категориям. Имя каждого шрифта показано его \
             собственным начертанием.",
        );
        ui.add_space(4.0);

        // Move the categories out so the collapsing-header closures can mutate `self`
        // (e.g. open the picker) without aliasing the borrowed lists.
        let categories = self.categories.take();
        match &categories {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Загрузка списков шрифтов…");
                });
            }
            Some(cats) => self.draw_categories(ui, cats),
        }
        self.categories = categories;

        self.draw_import_picker(ui.ctx());
    }

    /// Renders the three category collapsing headers from a loaded snapshot.
    fn draw_categories(&mut self, ui: &mut egui::Ui, cats: &FontCategories) {
        egui::CollapsingHeader::new(format!("Шрифты из папки fonts ({})", cats.folder.len()))
            .id_salt("font_settings_folder")
            .default_open(false)
            .show(ui, |ui| {
                if cats.folder.is_empty() {
                    ui.small("Папка fonts пуста.");
                } else {
                    Self::draw_font_rows_virtualized(ui, &cats.folder, "font_settings_folder_rows");
                }
            });

        egui::CollapsingHeader::new(format!(
            "Добавленные системные шрифты ({})",
            cats.imported.len()
        ))
        .id_salt("font_settings_imported")
        .default_open(false)
        .show(ui, |ui| {
            if cats.imported.is_empty() {
                ui.small("Пусто. Добавьте шрифты кнопкой ниже.");
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
                                    .on_hover_text("Удалить из импортированных")
                                    .clicked()
                                {
                                    font_settings_store::remove_imported_system_font(&font.path);
                                }
                                Self::draw_font_name_row(ui, font);
                            });
                        }
                    });
            }
            ui.add_space(4.0);
            // Kept OUTSIDE the scrolled row area so it stays reachable regardless of scroll.
            if ui.button("Импортировать шрифт из системы").clicked() {
                self.picker_open = true;
            }
        });

        egui::CollapsingHeader::new("Кастомные шрифты")
            .id_salt("font_settings_custom")
            .default_open(false)
            .show(ui, |ui| {
                // `custom` is intentionally empty for now; still read it so the field is
                // wired for the future virtual-font category.
                if cats.custom.is_empty() {
                    ui.small("Пока не поддерживаются.");
                } else {
                    Self::draw_font_rows_virtualized(ui, &cats.custom, "font_settings_custom_rows");
                }
            });
    }

    /// Draws a virtualized list of own-typeface font-name rows for a category. Only the rows
    /// currently visible are read and registered into egui per frame (egui `add_font` is
    /// non-evicting), so expanding a large `fonts/` folder no longer reads+registers all N
    /// fonts in a single frame. `id_salt` disambiguates sibling scroll areas.
    fn draw_font_rows_virtualized(ui: &mut egui::Ui, fonts: &[FontEntry], id_salt: &str) {
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
                    Self::draw_font_name_row(ui, font);
                }
            });
    }

    /// Draws one font's display name rendered in its OWN typeface. Registers the font's
    /// representative face into egui on first use (guarded by `is_font_family_bound`) and
    /// always restores the previous style font override. Falls back to the default font
    /// when the file cannot be registered; never panics.
    fn draw_font_name_row(ui: &mut egui::Ui, font: &FontEntry) {
        let rep_face = font.faces.first().map(|face| face.face_index).unwrap_or(0);
        let body_size = egui::TextStyle::Body.resolve(ui.style()).size;
        let prev_override = ui.style().override_font_id.clone();
        if let Some(family) = ensure_font_family(ui.ctx(), &font.path, rep_face) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(body_size, family));
        }
        ui.label(clean_font_display_name(&font.label));
        ui.style_mut().override_font_id = prev_override;
    }

    /// Starts a background category load if none is cached/in-flight OR the cached snapshot
    /// is stale (its `loaded_revision` differs from the current store revision). Reads the
    /// imported paths + revision on the GUI thread (cheap) and does the heavy folder scan +
    /// entry building on a worker thread.
    fn maybe_start_categories_load(&mut self) {
        if self.categories_rx.is_some() {
            return;
        }
        let current_revision = font_settings_store::imported_fonts_revision();
        let stale = self
            .categories
            .as_ref()
            .is_none_or(|cats| cats.loaded_revision != current_revision);
        if !stale {
            return;
        }

        let imported_paths = font_settings_store::imported_system_fonts();
        let (tx, rx) = mpsc::channel();
        match thread::Builder::new()
            .name("settings-load-font-categories".to_string())
            .spawn(move || {
                let folder = load_fonts_from_dir(&resolve_fonts_dir());
                let imported = load_imported_system_fonts(&imported_paths);
                let snapshot = FontCategories {
                    folder,
                    imported,
                    custom: Vec::new(),
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
                // empty snapshot at the current revision as a best-effort fallback.
                self.categories = Some(FontCategories {
                    folder: Vec::new(),
                    imported: Vec::new(),
                    custom: Vec::new(),
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

        egui::Window::new("Импорт системного шрифта")
            .open(&mut window_open)
            .collapsible(false)
            .resizable(true)
            .default_width(440.0)
            .show(ctx, |ui| match &catalog {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Загрузка списка системных шрифтов…");
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
            && !font_settings_store::add_imported_system_font(path.clone())
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
                let _ = tx.send(load_system_fonts());
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
        ui.label("Поиск:");
        ui.add(
            egui::TextEdit::singleline(search)
                .desired_width(300.0)
                .hint_text("имя шрифта"),
        );
    });
    ui.add_space(4.0);

    // Filter once per frame; only the indices survive so virtualization can index back.
    let filtered: Vec<usize> = fonts
        .iter()
        .enumerate()
        .filter(|(_, font)| font_row_matches(&font.label, &font.original_name, search))
        .map(|(idx, _)| idx)
        .collect();

    if filtered.is_empty() {
        ui.small("Ничего не найдено.");
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
                    let is_selected = selected.as_deref() == Some(font.path.as_path());
                    let rep_face = font.faces.first().map(|face| face.face_index).unwrap_or(0);
                    // Preview this row in its own typeface only if the family is already bound,
                    // already previewed this session, or we are still under the cap. Beyond the
                    // cap we render in the default font: egui `add_font` never evicts, so an
                    // unbounded catalog scroll would otherwise leak hundreds of MB of atlases.
                    let font_name = combo_font_family_name(&font.path, rep_face);
                    let allow_own = is_font_family_bound(
                        ui.ctx(),
                        &egui::FontFamily::Name(font_name.clone().into()),
                    ) || preview_families.contains(&font_name)
                        || preview_families.len() < PICKER_PREVIEW_FONT_CAP;
                    let prev_override = ui.style().override_font_id.clone();
                    if allow_own
                        && let Some(family) = ensure_font_family(ui.ctx(), &font.path, rep_face)
                    {
                        ui.style_mut().override_font_id =
                            Some(egui::FontId::new(body_size, family));
                        preview_families.insert(font_name);
                    }
                    let response =
                        ui.selectable_label(is_selected, clean_font_display_name(&font.label));
                    ui.style_mut().override_font_id = prev_override;
                    if response.clicked() {
                        *selected = Some(font.path.clone());
                    }
                }
            });
    }

    ui.separator();
    let already_imported = selected.as_ref().is_some_and(|path| {
        font_settings_store::imported_system_fonts()
            .iter()
            .any(|existing| existing == path)
    });
    ui.horizontal(|ui| {
        let can_add = selected.is_some() && !already_imported;
        if ui
            .add_enabled(can_add, egui::Button::new("Добавить"))
            .clicked()
        {
            *to_add = selected.clone();
            *close_requested = true;
        }
        if ui.button("Отмена").clicked() {
            *close_requested = true;
        }
        if already_imported {
            ui.label("Уже добавлен.");
        }
    });
}

/// Ensures the font at `font_path` (representative `face_index`) is registered as an egui
/// font family and returns it. Reuses an existing binding (shared with the create/edit
/// panels via the deterministic `combo_font_family_name`), so a font already registered
/// elsewhere is not read again. Returns `None` when the file cannot be read or egui does
/// not bind it. Reads the font file on the GUI thread ON FIRST USE ONLY, exactly like
/// `create_presets::ensure_combo_font_family` — registration inherently needs the bytes.
fn ensure_font_family(
    ctx: &egui::Context,
    font_path: &Path,
    face_index: usize,
) -> Option<egui::FontFamily> {
    let font_name = combo_font_family_name(font_path, face_index);
    let family = egui::FontFamily::Name(font_name.clone().into());
    if is_font_family_bound(ctx, &family) {
        return Some(family);
    }

    let font_bytes = fs::read(font_path).ok()?;
    let mut font_data = egui::FontData::from_owned(font_bytes);
    font_data.index = u32::try_from(face_index).unwrap_or(0);
    ctx.add_font(egui::epaint::text::FontInsert::new(
        font_name.as_str(),
        font_data,
        vec![egui::epaint::text::InsertFontFamily {
            family: family.clone(),
            priority: egui::epaint::text::FontPriority::Highest,
        }],
    ));
    is_font_family_bound(ctx, &family).then_some(family)
}

/// Case-insensitive substring match of a font row against a search query. Empty/whitespace
/// query matches everything. Matches either the display `label` or the `original_name`.
fn font_row_matches(label: &str, original_name: &str, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    label.to_lowercase().contains(&needle) || original_name.to_lowercase().contains(&needle)
}

/// Strips the internal `" [system]"` marker from a font label for display. The `" (N)"`
/// duplicate-disambiguation suffix (if any) is preserved so distinct files stay distinct.
fn clean_font_display_name(label: &str) -> String {
    label.replace(" [system]", "")
}

#[cfg(test)]
mod tests {
    use super::{clean_font_display_name, font_row_matches};

    #[test]
    fn empty_query_matches_everything() {
        assert!(font_row_matches("Arial", "Arial", ""));
        assert!(font_row_matches("Arial", "Arial", "   "));
    }

    #[test]
    fn query_matches_label_case_insensitively() {
        assert!(font_row_matches("Roboto-Bold [system]", "Roboto", "roboto"));
        assert!(font_row_matches("Roboto-Bold [system]", "Roboto", "BOLD"));
    }

    #[test]
    fn query_matches_original_name_when_label_differs() {
        // Label is a file stem; the real family name only lives in `original_name`.
        assert!(font_row_matches("DejaVuSans", "DejaVu Sans", "dejavu sans"));
    }

    #[test]
    fn non_matching_query_is_rejected() {
        assert!(!font_row_matches("Arial", "Arial", "comic"));
    }

    #[test]
    fn clean_display_name_strips_system_marker_keeps_dedup_suffix() {
        assert_eq!(clean_font_display_name("Roboto [system]"), "Roboto");
        assert_eq!(clean_font_display_name("Roboto [system] (2)"), "Roboto (2)");
        // A plain folder-font label is unchanged.
        assert_eq!(clean_font_display_name("Comic Sans"), "Comic Sans");
    }
}
