/*
FILE HEADER (tabs/typing/font_admin.rs)

Purpose:
The ONE sanctioned entry point for NON-typing code (currently the settings
"Настройки шрифтов" UI in `src/tabs/settings/typesetting/`) to read and mutate the
app's font administration state. Typing owns the font MODEL — the loaders, the
per-font settings store, the on-disk `fonts_data.json` schema, and the
`FontEntry`/`FontFaceEntry` types. Those stay `pub(in crate::tabs::typing)`; this
module is the only place that widens a NARROW, wrapped surface of them to `pub(crate)`.

Contract:
- External callers import ONLY this module (`crate::tabs::typing::font_admin`). No other
  typing internal is `pub(crate)`.
- `FontEntry` is re-exported as an OPAQUE type: its fields and constructors stay private
  to typing; external readers use the `pub(crate)` accessors on `FontEntry` itself.
- Heavy font enumeration (`load_folder_fonts` / `load_imported_fonts` /
  `load_system_catalog`) MUST run off the GUI thread — it walks the fonts dir / the OS
  font database.
- The per-font display-name override keying (`fonts_data::font_settings_key` +
  `resolve_fonts_dir`) is computed INSIDE this facade from a `&Path`, so the keying
  scheme stays fully private to typing.

Key functions:
- `load_folder_fonts` / `load_imported_fonts` / `load_system_catalog`
- `fonts_revision` / `add_imported_font` / `remove_imported_font` / `is_font_imported`
- `display_name_override` / `set_display_name_override`
*/

use std::path::{Path, PathBuf};

use super::panel::{fonts, fonts_data, font_settings_store};
// Re-exported crate-wide as an OPAQUE type (fields/constructors stay private to typing);
// external readers use the `pub(crate)` accessors on `FontEntry`.
pub(crate) use super::panel::FontEntry;

/// Loads the fonts discovered in the project `fonts/` folder. HEAVY (directory walk);
/// run off the GUI thread.
#[must_use]
pub(crate) fn load_folder_fonts() -> Vec<FontEntry> {
    fonts::load_fonts_from_dir(&fonts::resolve_fonts_dir())
}

/// Loads the user-imported system fonts (snapshotting the store's paths, then building the
/// entries). HEAVY (per-font file parse); run off the GUI thread.
#[must_use]
pub(crate) fn load_imported_fonts() -> Vec<FontEntry> {
    let paths = font_settings_store::imported_system_fonts();
    fonts::load_imported_system_fonts(&paths)
}

/// Enumerates ALL OS-installed fonts — the catalog for the system-font import picker.
/// VERY HEAVY (whole OS font database); run off the GUI thread.
#[must_use]
pub(crate) fn load_system_catalog() -> Vec<FontEntry> {
    fonts::load_system_fonts()
}

/// Current revision of the imported-fonts store; advances on any add/remove/override so a
/// cached font list can detect staleness. Cheap; may be polled from the GUI thread.
#[must_use]
pub(crate) fn fonts_revision() -> u64 {
    font_settings_store::imported_fonts_revision()
}

/// Imports `path` as a system font. Returns `false` when it was already imported (a no-op).
/// Persists off the GUI thread and bumps the store revision.
pub(crate) fn add_imported_font(path: PathBuf) -> bool {
    font_settings_store::add_imported_system_font(path)
}

/// Removes a previously-imported system font by its file `path`. Returns `false` when it was
/// not imported. Persists off the GUI thread and bumps the store revision.
pub(crate) fn remove_imported_font(path: &Path) -> bool {
    font_settings_store::remove_imported_system_font(path)
}

/// Whether `path` is currently in the imported-system-fonts set. Cheap; GUI-thread safe.
#[must_use]
pub(crate) fn is_font_imported(path: &Path) -> bool {
    font_settings_store::imported_system_fonts()
        .iter()
        .any(|existing| existing == path)
}

/// Reads the user display-name override for the font at `path`, if any. The stable per-font
/// key is derived internally, so the keying scheme stays private to typing.
#[must_use]
pub(crate) fn display_name_override(path: &Path) -> Option<String> {
    font_settings_store::font_display_name_override(&key_for(path))
}

/// Sets (or, with `None`, clears) the user display-name override for the font at `path`.
/// Returns whether the stored value changed. Persists off the GUI thread and bumps the
/// store revision, so cached font lists reload.
pub(crate) fn set_display_name_override(path: &Path, value: Option<String>) -> bool {
    font_settings_store::set_font_display_name_override(&key_for(path), value)
}

/// Derives the stable per-font settings KEY for `path` (fonts-dir-relative when under the
/// fonts dir, else absolute). Keeps `resolve_fonts_dir` + the keying scheme private to typing.
fn key_for(path: &Path) -> String {
    fonts_data::font_settings_key(&fonts::resolve_fonts_dir(), path)
}
