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
- virtual font groups (config-only named font sets): `list_virtual_groups` /
  `create_virtual_group` / `delete_virtual_group` / `rename_virtual_group` /
  `add_virtual_group_member` / `remove_virtual_group_member` /
  `set_virtual_group_member_alias` / `virtual_groups_for_font`
- `list_folder_group_names` (real folder groups under `fonts/groups/`; HEAVY, off-thread)

Key mapping:
- Virtual-group members are stored by `font_settings_key`; the facade converts `&Path` ->
  key (`key_for`) inbound and key -> `PathBuf` (`path_for_key`) outbound, so the keying
  scheme never leaks to external callers.
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

/// Resolves a `font_settings_key` back to a filesystem path: an absolute key is used
/// verbatim, otherwise it is joined onto the fonts directory (the inverse of `key_for`).
/// Keeps the keying scheme + `resolve_fonts_dir` private to typing.
fn path_for_key(key: &str) -> PathBuf {
    let candidate = Path::new(key);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        fonts::resolve_fonts_dir().join(candidate)
    }
}

/// A virtual font group exposed to non-typing code: its name and members, with each member
/// referenced by a real filesystem `path` (keys are resolved internally, never leaked).
#[derive(Debug, Clone)]
pub(crate) struct VirtualFontGroupInfo {
    /// Group display name.
    pub(crate) name: String,
    /// Ordered members (user order preserved).
    pub(crate) members: Vec<VirtualFontGroupMemberInfo>,
}

/// One member of a [`VirtualFontGroupInfo`]: the referenced font's filesystem path plus its
/// optional per-group display alias.
#[derive(Debug, Clone)]
pub(crate) struct VirtualFontGroupMemberInfo {
    /// Filesystem path of the referenced real font (resolved from its stable key).
    pub(crate) path: PathBuf,
    /// Optional per-group display alias; `None` means "use the font's own label".
    pub(crate) alias: Option<String>,
}

/// Lists all virtual font groups, with each member's stable key resolved to a filesystem
/// `path`. Cheap (in-memory snapshot); GUI-thread safe.
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
                    path: path_for_key(&member.font),
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

/// Adds the font at `path` to virtual group `group`. Returns `false` when the group is
/// unknown or the font is already a member. Keys are derived internally so the keying scheme
/// stays private. Persists off the GUI thread and bumps the store revision.
pub(crate) fn add_virtual_group_member(group: &str, path: &Path) -> bool {
    font_settings_store::add_virtual_group_member(group, &key_for(path))
}

/// Removes the font at `path` from virtual group `group`. Returns `false` when the group is
/// unknown or the font was not a member. Persists off the GUI thread and bumps the store revision.
pub(crate) fn remove_virtual_group_member(group: &str, path: &Path) -> bool {
    font_settings_store::remove_virtual_group_member(group, &key_for(path))
}

/// Sets (or, with `None`/blank, clears) the per-group display alias of the font at `path`
/// in virtual group `group`. Returns `false` when the group/member is missing or the alias
/// is unchanged. Persists off the GUI thread and bumps the store revision.
pub(crate) fn set_virtual_group_member_alias(
    group: &str,
    path: &Path,
    alias: Option<&str>,
) -> bool {
    font_settings_store::set_virtual_group_member_alias(group, &key_for(path), alias)
}

/// Returns, for the font at `path`, every virtual group that contains it as `(group name,
/// per-group alias)`. For the font properties window. Cheap (in-memory scan); GUI-thread safe.
#[must_use]
pub(crate) fn virtual_groups_for_font(path: &Path) -> Vec<(String, Option<String>)> {
    let key = key_for(path);
    font_settings_store::virtual_groups()
        .into_iter()
        .filter_map(|group| {
            group
                .members
                .into_iter()
                .find(|member| member.font == key)
                .map(|member| (group.name, member.alias))
        })
        .collect()
}

/// Lists the real FOLDER-group names discovered under `fonts/groups/`. HEAVY: performs
/// filesystem I/O (one `read_dir` of the groups directory) — callers should invoke it from
/// their existing off-thread font loads, not per frame on the GUI thread.
#[must_use]
pub(crate) fn list_folder_group_names() -> Vec<String> {
    fonts::load_font_groups(&fonts::resolve_fonts_dir())
}
