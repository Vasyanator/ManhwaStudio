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
- Heavy font enumeration (`load_font_lists` / `load_system_catalog`) MUST run off the GUI
  thread — it walks the fonts dir / the OS font database.
- The font lists are built as ONE combined pass, so the identities the settings UI writes
  (group membership, display-name overrides) are the identities the typing panel resolves.
- `#[cfg(test)]` `test_lock` / `test_reset` widen the store's test harness the same narrow
  way, so a settings-UI test can drive the real store without importing a typing internal.

Key functions:
- `load_font_lists` (folder + imported in ONE identity-consistent pass) / `load_system_catalog`
- `flush_pending_saves` (app-exit flush of the debounced per-font-settings write)
- `fonts_revision` / `add_imported_font` / `remove_imported_font` / `is_font_imported`
- `display_name_override` / `set_display_name_override`
- virtual font groups (config-only named font sets): `list_virtual_groups` /
  `create_virtual_group` / `delete_virtual_group` / `rename_virtual_group` /
  `add_virtual_group_member` / `remove_virtual_group_member` /
  `set_virtual_group_member_alias` / `virtual_groups_for_font`
- `list_folder_group_names` (real folder groups under `fonts/groups/`; HEAVY, off-thread)

Key mapping:
- EVERY per-font setting is keyed by the font's IDENTITY
  (`FontEntry::render_identity_name` — the representative face's PostScript name), so the
  facade takes an `&str` identity, never a path. The ONE exception is importing a system
  font: that inherently starts from a FILE the user picked, so `add_imported_font` takes
  both the identity to store and the path to remember as the byte-source hint.
*/

use std::path::PathBuf;

use super::panel::{fonts, font_settings_store};
// Re-exported crate-wide as an OPAQUE type (fields/constructors stay private to typing);
// external readers use the `pub(crate)` accessors on `FontEntry`.
pub(crate) use super::panel::FontEntry;

/// Why an imported system font recorded in `fonts_data.json` could not be loaded this run.
/// The UI maps each variant to a localized "unavailable" note on the row.
///
/// The variant describes what happened at the RECORDED PATH. A row carries one only when the
/// font could not be located by NAME either (that lookup is the loader's second step); a font
/// that merely moved comes back as an available row instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportedFontUnavailability {
    /// The document records no file path for it (nothing to read).
    NoPathHint,
    /// The recorded file is missing or unreadable; carries the OS error for the tooltip.
    Unreadable(String),
    /// The file exists but no font parser accepts it.
    Unparsable,
    /// The file now holds a DIFFERENT font than the one that was imported.
    NameMismatch {
        /// The name the file claims today.
        found: String,
    },
}

/// One row of the settings "imported system fonts" list: what the DOCUMENT records, plus the
/// loaded font when its file could be used.
///
/// EVERY stored entry produces a row, including the ones that could not be loaded. That is
/// the point: an imported font whose file went missing used to be skipped by the loader, so
/// no row existed, nothing pruned the document, and the entry became permanently
/// unremovable.
pub(crate) struct ImportedFontRow {
    /// The identity stored in `fonts_data.json` — exactly the key [`remove_imported_font`]
    /// matches. NOT necessarily the loaded font's render identity, which may carry a
    /// collision suffix.
    pub(crate) stored_identity: String,
    /// The recorded file path, if any. Display/diagnostics only — never a key.
    pub(crate) last_path: Option<std::path::PathBuf>,
    /// The loaded font, or `None` when it is unavailable.
    pub(crate) font: Option<FontEntry>,
    /// Why the font is unavailable; `None` exactly when `font` is `Some`.
    pub(crate) unavailable: Option<ImportedFontUnavailability>,
}

/// The font-administration lists, built in ONE combined pass.
///
/// `folder` and `imported` come from the SAME merged list, so every font in them carries the
/// identity the typing panel uses. Building the two categories independently made the
/// settings pane show (and write) bare identities where the panel had assigned suffixed ones,
/// which silently broke group membership and display-name overrides for colliding fonts.
pub(crate) struct FontAdminLists {
    /// Fonts whose file lives in the project `fonts/` folder.
    pub(crate) folder: Vec<FontEntry>,
    /// The loadable imported system fonts, as list entries (for the group editor's pickers).
    pub(crate) imported: Vec<FontEntry>,
    /// One row per stored imported system font, in document order, including unavailable ones.
    pub(crate) imported_rows: Vec<ImportedFontRow>,
}

/// Loads the folder fonts and the imported system fonts as ONE identity-consistent snapshot.
/// HEAVY (directory walk plus a parse per imported file); run off the GUI thread.
#[must_use]
pub(crate) fn load_font_lists() -> FontAdminLists {
    let fonts_dir = fonts::resolve_fonts_dir();
    let refs = font_settings_store::imported_system_font_refs();
    let combined = fonts::build_combined_font_list(&fonts_dir, &refs);
    // A folder font is one whose representative FILE lives under the fonts dir. After the
    // duplicate fold the representative of a folder+imported pair is always the folder copy
    // (folder entries are appended first), so this split cannot misfile a merged font.
    let folder: Vec<FontEntry> = combined
        .entries
        .iter()
        .filter(|entry| entry.path().starts_with(&fonts_dir))
        .cloned()
        .collect();
    let imported_rows: Vec<ImportedFontRow> = combined
        .imported_rows
        .into_iter()
        .map(|row| ImportedFontRow {
            stored_identity: row.stored_identity,
            last_path: row.last_path,
            font: row.entry,
            unavailable: row.unavailable.map(|reason| match reason {
                fonts::ImportedFontUnavailable::NoPathHint => {
                    ImportedFontUnavailability::NoPathHint
                }
                fonts::ImportedFontUnavailable::Unreadable(error) => {
                    ImportedFontUnavailability::Unreadable(error)
                }
                fonts::ImportedFontUnavailable::Unparsable => {
                    ImportedFontUnavailability::Unparsable
                }
                fonts::ImportedFontUnavailable::NameMismatch { found } => {
                    ImportedFontUnavailability::NameMismatch { found }
                }
            }),
        })
        .collect();
    // The loadable imported entries, MINUS the ones the fold already put in `folder`: an
    // imported file that is a byte-identical copy of a folder font is represented by that
    // folder entry, and listing it in both categories would double it in the group-editor's
    // add picker (which chains the two). Its removable ROW still exists, unaffected.
    let folder_identities: std::collections::HashSet<String> =
        folder.iter().map(FontEntry::render_identity_name).collect();
    let imported: Vec<FontEntry> = imported_rows
        .iter()
        .filter_map(|row| row.font.clone())
        .filter(|font| !folder_identities.contains(&font.render_identity_name()))
        .collect();
    FontAdminLists {
        folder,
        imported,
        imported_rows,
    }
}

/// Writes a still-pending debounced per-font-settings save immediately, on the calling
/// thread. Returns whether there was anything to flush. Called from the app's `on_exit`: a
/// parameter edit persists through a multi-second debounce, and closing the app inside that
/// window would otherwise lose it.
pub(crate) fn flush_pending_saves() -> bool {
    font_settings_store::flush_pending_saves()
}

/// Enumerates ALL OS-installed fonts — the catalog for the system-font import picker.
/// VERY HEAVY (whole OS font database); run off the GUI thread.
///
/// SIDE EFFECT: the enumeration is also published as the process-wide `PostScript name → file`
/// index used to locate an imported system font that moved, so opening the picker is what
/// refreshes that index after the user installs or removes fonts.
#[must_use]
pub(crate) fn load_system_catalog() -> Vec<FontEntry> {
    fonts::load_system_fonts()
}

/// Current revision of the font-settings store; advances on any add/remove/override/group
/// change so a cached font list can detect staleness. Cheap; may be polled from the GUI
/// thread.
#[must_use]
pub(crate) fn fonts_revision() -> u64 {
    font_settings_store::imported_fonts_revision()
}

/// Imports the system font `identity` (its PostScript name), recording `path` as the hint of
/// where its bytes were last seen. Returns `false` when that font was already imported (a
/// no-op) or when `identity` is blank. Persists off the GUI thread and bumps the store
/// revision.
///
/// The path is deliberately part of the input here and nowhere else: importing starts from
/// a FILE the user picked in a system-font picker, while everything stored about the font
/// afterwards is keyed by its name.
pub(crate) fn add_imported_font(identity: &str, path: PathBuf) -> bool {
    font_settings_store::add_imported_system_font(identity, path)
}

/// Removes a previously-imported system font by its IDENTITY. Returns `false` when it was
/// not imported. Persists off the GUI thread and bumps the store revision.
pub(crate) fn remove_imported_font(identity: &str) -> bool {
    font_settings_store::remove_imported_system_font(identity)
}

/// Whether a system font with this IDENTITY is currently imported. Cheap; GUI-thread safe.
#[must_use]
pub(crate) fn is_font_imported(identity: &str) -> bool {
    font_settings_store::is_system_font_imported(identity)
}

/// Reads the user display-name override for the font `identity`, if any.
#[must_use]
pub(crate) fn display_name_override(identity: &str) -> Option<String> {
    font_settings_store::font_display_name_override(identity)
}

/// Sets (or, with `None`, clears) the user display-name override for the font `identity`.
/// Returns whether the stored value changed. Persists off the GUI thread and bumps the
/// store revision, so cached font lists reload.
pub(crate) fn set_display_name_override(identity: &str, value: Option<String>) -> bool {
    font_settings_store::set_font_display_name_override(identity, value)
}

/// A virtual font group exposed to non-typing code: its name and members, with each member
/// referenced by the font's IDENTITY.
#[derive(Debug, Clone)]
pub(crate) struct VirtualFontGroupInfo {
    /// Group display name.
    pub(crate) name: String,
    /// Ordered members (user order preserved).
    pub(crate) members: Vec<VirtualFontGroupMemberInfo>,
}

/// One member of a [`VirtualFontGroupInfo`]: the referenced font's IDENTITY plus its
/// optional per-group display alias.
#[derive(Debug, Clone)]
pub(crate) struct VirtualFontGroupMemberInfo {
    /// IDENTITY of the referenced real font (`FontEntry::render_identity_name`). A member
    /// left over from an unmigrated legacy document may still hold a path-shaped string; it
    /// simply resolves to no loaded font and is shown as unavailable.
    pub(crate) font: String,
    /// Optional per-group display alias; `None` means "use the font's own label".
    pub(crate) alias: Option<String>,
}

/// Lists all virtual font groups. Cheap (in-memory snapshot); GUI-thread safe.
#[must_use]
pub(crate) fn list_virtual_groups() -> Vec<VirtualFontGroupInfo> {
    font_settings_store::virtual_groups()
        .into_iter()
        .map(|group| VirtualFontGroupInfo {
            name: group.name,
            members: group
                .members
                .into_iter()
                .map(|member| VirtualFontGroupMemberInfo {
                    font: member.font,
                    alias: member.alias,
                })
                .collect(),
        })
        .collect()
}

/// Creates an empty virtual font group. Returns `false` when the name is blank or a
/// case-insensitive duplicate of an existing VIRTUAL group. Persists off the GUI thread and
/// bumps the store revision. Does NOT reject a collision with a real FOLDER-group name — the
/// UI validates that (the store cannot see the filesystem).
pub(crate) fn create_virtual_group(name: &str) -> bool {
    font_settings_store::create_virtual_group(name)
}

/// Deletes the virtual group named exactly `name`. Returns `false` when none matched.
/// Persists off the GUI thread and bumps the store revision.
pub(crate) fn delete_virtual_group(name: &str) -> bool {
    font_settings_store::delete_virtual_group(name)
}

/// Renames virtual group `old` to `new`. Returns `false` when `new` is blank, `old` is
/// missing, the name is unchanged, or `new` collides case-insensitively with another group.
/// Persists off the GUI thread and bumps the store revision.
pub(crate) fn rename_virtual_group(old: &str, new: &str) -> bool {
    font_settings_store::rename_virtual_group(old, new)
}

/// Adds the font `identity` to virtual group `group`. Returns `false` when the group is
/// unknown or the font is already a member. Persists off the GUI thread and bumps the store
/// revision.
pub(crate) fn add_virtual_group_member(group: &str, identity: &str) -> bool {
    font_settings_store::add_virtual_group_member(group, identity)
}

/// Removes the font `identity` from virtual group `group`. Returns `false` when the group is
/// unknown or the font was not a member. Persists off the GUI thread and bumps the store revision.
pub(crate) fn remove_virtual_group_member(group: &str, identity: &str) -> bool {
    font_settings_store::remove_virtual_group_member(group, identity)
}

/// Sets (or, with `None`/blank, clears) the per-group display alias of the font `identity`
/// in virtual group `group`. Returns `false` when the group/member is missing or the alias
/// is unchanged. Persists off the GUI thread and bumps the store revision.
pub(crate) fn set_virtual_group_member_alias(
    group: &str,
    identity: &str,
    alias: Option<&str>,
) -> bool {
    font_settings_store::set_virtual_group_member_alias(group, identity, alias)
}

/// Returns, for the font `identity`, every virtual group that contains it as `(group name,
/// per-group alias)`. For the font properties window. Cheap (in-memory scan); GUI-thread safe.
#[must_use]
pub(crate) fn virtual_groups_for_font(identity: &str) -> Vec<(String, Option<String>)> {
    font_settings_store::virtual_groups_for_font(identity)
}

/// Lists the real FOLDER-group names discovered under `fonts/groups/`. HEAVY: performs
/// filesystem I/O (one `read_dir` of the groups directory) — callers should invoke it from
/// their existing off-thread font loads, not per frame on the GUI thread.
#[must_use]
pub(crate) fn list_folder_group_names() -> Vec<String> {
    fonts::load_font_groups(&fonts::resolve_fonts_dir())
}

/// Serializes every test that touches the PROCESS-GLOBAL font-settings store, across ALL
/// crates' test modules — the store is one static, so a settings-UI test and a typing-model
/// test that both reset it would otherwise race. Test-only; part of the facade so external
/// tests never reach for a typing internal.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    font_settings_store::test_lock()
}

/// Clears the shared font-settings store to a known-empty baseline for an isolated test.
/// Callers MUST hold [`test_lock`]. Test-only; see the store's own `test_reset` for exactly
/// what is (and is not) reset.
#[cfg(test)]
pub(crate) fn test_reset() {
    font_settings_store::test_reset();
}
