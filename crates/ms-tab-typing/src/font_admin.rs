/*
FILE HEADER (tabs/typing/font_admin.rs)

Purpose:
The ONE sanctioned entry point for NON-typing code (currently the settings
"Настройки шрифтов" UI in `src/tabs/settings/typesetting/`) to read and mutate the
app's font administration state. Typing owns the font MODEL — the loaders, the
per-font settings store, the on-disk `fonts_data.json` schema, and the
`FontEntry`/`FontFaceEntry` types. Those stay `pub(crate)`; this
module is the only place that widens a NARROW, wrapped surface of them to `pub(crate)`.

Contract:
- External callers import ONLY this module (`crate::font_admin`). No other
  typing internal is `pub(crate)`.
- `FontEntry` is re-exported as an OPAQUE type: its fields and constructors stay private
  to typing; external readers use the `pub(crate)` accessors on `FontEntry` itself.
- Font BYTES are located only through `FontEntry::byte_source()` (re-exported
  `FontByteSource`, matched exhaustively; its `Custom` arm names a `CustomFontRef`);
  `FontEntry::file_path()` answers location questions (folder, import hint, file name
  shown) and is never read for bytes. Own-typeface previews go through
  `FontByteSource::preview_source()` -> `FontPreviewSource::request_family`; a worker that
  needs the bytes themselves calls `font_bytes_blocking`.
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
- BULK entry points for an import that adds many fonts at once — `add_imported_fonts` /
  `add_virtual_group_members` (ONE revision bump + ONE document write per batch), plus
  `locate_system_font_by_identity` (find an INSTALLED font by PostScript name; BLOCKING,
  off-thread only) and the pure `is_valid_post_script_name` check
- IDENTITY COMPARISON: `normalize_font_identity` (trim + ASCII lowercase — THE rule every
  identity lookup in the app uses) and `is_bundled_ui_font_identity` (the synthetic bundled
  interface-font entry is NOT in `load_font_lists`, so availability checks must know it)
- `display_name_override` / `set_display_name_override`
- CUSTOM KERNING (`custom_kerning` / `set_custom_kerning`, plus the re-exported
  `CustomKerningPair`): user-authored kerning pair overrides for one font, measured in
  THOUSANDTHS OF AN EM so they are independent of the rendered size and of the font's
  `units_per_em`. A pair of `0.0` is meaningful — it CANCELS a built-in pair — and is never
  filtered out. The setter bumps the fonts revision, which is how an edit reaches the
  renderer (the panel reloads its fonts and the provider re-publishes the pairs); the font
  FILE is never modified. `FontEntry::custom_kerning()` reads the pairs resolved onto a
  loaded font
- virtual font groups (config-only named font sets): `list_virtual_groups` /
  `create_virtual_group` / `delete_virtual_group` / `rename_virtual_group` /
  `add_virtual_group_member` / `remove_virtual_group_member` /
  `set_virtual_group_member_alias` / `virtual_groups_for_font`
- `list_folder_group_names` (real folder groups under `fonts/groups/`; HEAVY, off-thread)
- SAVED CUSTOM FONTS IN THE FONT LISTS: `FontAdminLists::custom` (one `CustomFontRow` per
  stored font, unavailable ones included, with the localized reason
  `CustomFontUnavailability::tooltip`), `is_custom_font_identity`, `font_bytes_blocking`
- SAVED CUSTOM FONTS (recipes, e.g. hybrids): `list_custom_fonts` (-> `CustomFontInfo`, a
  facade-owned mirror like `VirtualFontGroupInfo`) / `custom_font_name` (one font's stored
  name, for rows whose font is not loadable) / `create_custom_font` (returns the minted
  saved identity) / `update_custom_font_recipe` / `rename_custom_font` / `delete_custom_font`
  (also drops the font's own kerning record and group memberships), errors
  `CustomFontStoreError`; the recipe model is re-exported from `ms_custom_fonts`
  (`CustomFontRecipe`, `CustomFontKind`, `HybridParams`, `HybridSource`,
  `MissingGlyphPolicy`). A stored entry this build cannot interpret is listed with
  `CustomFontInfo::unsupported` set and may only be deleted
- THE SHARED HYBRID EDITOR (`crate::hybrid_editor`, re-exported): `HybridEditorState`,
  `HybridEditorCx`, `HybridEditorOutcome`, `CommittedRecipe`, `CommitButton`, `DraftError`,
  `SourceCatalog`, `draw_hybrid_editor`, plus `hybrid_source_catalog` (the settings host's
  source catalog over the folder + imported fonts and the bundled interface font)

Key mapping:
- EVERY per-font setting is keyed by the font's IDENTITY
  (`FontEntry::render_identity_name` — the representative face's PostScript name), so the
  facade takes an `&str` identity, never a path. The ONE exception is importing a system
  font: that inherently starts from a FILE the user picked, so `add_imported_font` takes
  both the identity to store and the path to remember as the byte-source hint.
- Identities are COMPARED through `normalize_font_identity` (trim + ASCII lowercase) and
  never byte-for-byte: documents legitimately carry an identity in a different casing than
  the loaded font declares (the deferred legacy-key migration leaves such keys alone on
  purpose), and the typing panel resolves them normalized.
*/

use std::path::PathBuf;
use std::sync::Arc;

use super::panel::custom_font_entry::CustomFontUnavailable;
use super::panel::{fonts, font_settings_store, fonts_data};
// Re-exported crate-wide as an OPAQUE type (fields/constructors stay private to typing);
// external readers use the `pub(crate)` accessors on `FontEntry`.
pub use super::panel::FontEntry;
// THE answer to "where do this font's bytes come from" (`FontEntry::byte_source`). Every
// byte read of a font outside typing (the settings own-typeface previews and the
// font-properties analysis) matches it instead of reading `FontEntry::file_path`.
pub use super::panel::FontByteSource;
// What an own-typeface preview draws from (`FontByteSource::preview_source`), and the
// custom-font payload of `FontByteSource::Custom` (opaque).
pub use super::panel::{CustomFontRef, FontPreviewSource};
// Bounds the egui fonts registered for a saved custom font while its recipe is tuned: the
// settings properties window holds the pin (`custom_font_entry::pin_custom_face_display`).
pub use super::panel::custom_font_entry::{CustomFaceDisplayPin, pin_custom_face_display};
// A font's whole byte buffer, as `font_bytes_blocking` returns it.
pub use crate::render_next::FontBytes;
// Re-exported with its FIELDS, unlike `FontEntry`: the settings font-properties window must
// CONSTRUCT pairs to store them, and a mirror type here would be a third copy of a struct
// that carries no invariant beyond "finite offset" (enforced on store and on save).
pub use super::panel::fonts_data::CustomKerningPair;
// The custom-font recipe model, owned by `ms-custom-fonts`: the settings editor builds and
// reads recipes with these types, so a mirror here would only be a lossy copy.
pub use ms_custom_fonts::{CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MissingGlyphPolicy};
// The typed refusal of every custom-font mutation; the settings UI maps its variants to
// localized messages.
pub use super::panel::font_settings_store::CustomFontStoreError;
// The shared hybrid editor body: the settings create dialog and properties window host it
// exactly as the typing dock does (`dev-docs/hybrid_fonts_plan.md` §3.3).
pub use crate::hybrid_editor::{
    CommitButton, CommittedRecipe, DraftError, HybridEditorCx, HybridEditorOutcome, HybridEditorState, SourceCatalog,
    draw_hybrid_editor,
};

/// Why an imported system font recorded in `fonts_data.json` could not be loaded this run.
/// The UI maps each variant to a localized "unavailable" note on the row.
///
/// The variant describes what happened at the RECORDED PATH. A row carries one only when the
/// font could not be located by NAME either (that lookup is the loader's second step); a font
/// that merely moved comes back as an available row instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedFontUnavailability {
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
pub struct ImportedFontRow {
    /// The identity stored in `fonts_data.json` — exactly the key [`remove_imported_font`]
    /// matches. NOT necessarily the loaded font's render identity, which may carry a
    /// collision suffix.
    pub stored_identity: String,
    /// The recorded file path, if any. Display/diagnostics only — never a key.
    pub last_path: Option<std::path::PathBuf>,
    /// The loaded font, or `None` when it is unavailable.
    pub font: Option<FontEntry>,
    /// Why the font is unavailable; `None` exactly when `font` is `Some`.
    pub unavailable: Option<ImportedFontUnavailability>,
}

/// The font-administration lists, built in ONE combined pass.
///
/// `folder` and `imported` come from the SAME merged list, so every font in them carries the
/// identity the typing panel uses. Building the two categories independently made the
/// settings pane show (and write) bare identities where the panel had assigned suffixed ones,
/// which silently broke group membership and display-name overrides for colliding fonts.
pub struct FontAdminLists {
    /// Fonts whose file lives in the project `fonts/` folder.
    pub folder: Vec<FontEntry>,
    /// The loadable imported system fonts, as list entries (for the group editor's pickers).
    pub imported: Vec<FontEntry>,
    /// One row per stored imported system font, in document order, including unavailable ones.
    pub imported_rows: Vec<ImportedFontRow>,
    /// One row per SAVED custom font, in identity order, including unavailable ones (a
    /// missing source, a kind this build cannot compile, a damaged recipe). Built from the
    /// same combined pass, so their sources are exactly the fonts listed above.
    pub custom: Vec<CustomFontRow>,
}

/// Why a saved custom font cannot be used this run (facade mirror of the model's reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomFontUnavailability {
    /// These source fonts (identities) are not installed or could not be read.
    MissingSources(Vec<String>),
    /// Created by a newer build: this build cannot compile its kind (carries the tag).
    UnsupportedKind(String),
    /// Its stored settings are damaged (carries the log-ready reason).
    InvalidRecipe(String),
}

impl CustomFontUnavailability {
    /// The localized explanation for the greyed row's tooltip. GUI thread (`t!`).
    #[must_use]
    pub fn tooltip(&self) -> String {
        match self {
            Self::MissingSources(sources) => {
                tf!("typing.fonts.custom_font_missing_source_tooltip", font = sources.join(", "))
            }
            Self::UnsupportedKind(_) => t!("typing.fonts.custom_font_unsupported_kind_tooltip").to_string(),
            Self::InvalidRecipe(_) => t!("typing.fonts.custom_font_invalid_recipe_tooltip").to_string(),
        }
    }
}

/// One saved custom font of the font lists: what the document records plus its entry when
/// it can be used.
pub struct CustomFontRow {
    /// The saved identity (`MSCustom-<16 hex>`): the key layers, groups and kerning use.
    pub identity: String,
    /// The stored, user-visible name.
    pub name: String,
    /// The usable list entry (for previews, group pickers, the properties window), or
    /// `None` when the font is unavailable.
    pub font: Option<FontEntry>,
    /// Why the font is unavailable; `None` exactly when `font` is `Some`.
    pub unavailable: Option<CustomFontUnavailability>,
}

/// Loads the folder fonts and the imported system fonts as ONE identity-consistent snapshot.
/// HEAVY (directory walk plus a parse per imported file); run off the GUI thread.
#[must_use]
pub fn load_font_lists() -> FontAdminLists {
    let fonts_dir = ms_config::storage_mode::app_fonts_dir();
    let refs = font_settings_store::imported_system_font_refs();
    let combined = fonts::build_combined_font_list(&fonts_dir, &refs);
    // A folder font is one whose representative FILE lives under the fonts dir. After the
    // duplicate fold the representative of a folder+imported pair is always the folder copy
    // (folder entries are appended first), so this split cannot misfile a merged font.
    let folder: Vec<FontEntry> = combined
        .entries
        .iter()
        .filter(|entry| entry.file_path().starts_with(&fonts_dir))
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
    let custom = combined
        .custom_rows
        .into_iter()
        .map(|row| CustomFontRow {
            identity: row.identity,
            name: row.name,
            font: row.entry,
            unavailable: row.unavailable.map(|reason| match reason {
                CustomFontUnavailable::MissingSources(sources) => CustomFontUnavailability::MissingSources(sources),
                CustomFontUnavailable::UnsupportedKind(kind) => CustomFontUnavailability::UnsupportedKind(kind),
                CustomFontUnavailable::InvalidRecipe(reason) => CustomFontUnavailability::InvalidRecipe(reason),
            }),
        })
        .collect();
    FontAdminLists {
        folder,
        imported,
        imported_rows,
        custom,
    }
}

/// The hybrid editor's source catalog for the settings host: the bundled interface font plus
/// every folder and imported font of `lists` (never a custom font, decision D4), with a
/// source-only resolver over the same fonts.
///
/// Cheap (entry clones, no file read — the resolver reads lazily on the editor's workers),
/// but call it where `lists` is built (the settings load worker) and share the `Arc`, so the
/// editor sees one catalog per font-list load.
#[must_use]
pub fn hybrid_source_catalog(lists: &FontAdminLists) -> Arc<SourceCatalog> {
    let real: Vec<FontEntry> = lists.folder.iter().chain(&lists.imported).cloned().collect();
    let sources = super::panel::custom_font_entry::hybrid_source_fonts(&real);
    let resolver = super::panel::custom_font_entry::source_only_resolver(&sources);
    Arc::new(SourceCatalog::from_entries(&sources, resolver))
}

/// Whether `identity` names a custom font: a saved custom font (`MSCustom-<16 hex>`) or
/// the typing panel's ad-hoc hybrid entry. Custom fonts are not in `folder` / `imported`,
/// so an availability check over those lists must consult this (and
/// `FontAdminLists::custom`). Pure and cheap; GUI-thread safe.
#[must_use]
pub fn is_custom_font_identity(identity: &str) -> bool {
    crate::custom_fonts::identity::is_saved_identity(identity)
        || crate::custom_fonts::identity::is_adhoc_identity(identity)
}

/// Why [`font_bytes_blocking`] could not produce a font's bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FontBytesError {
    /// The font file (or the bundled font) could not be read; carries the location and
    /// the OS reason.
    #[error("font file could not be read: {0}")]
    Unreadable(String),
    /// The custom font could not be built (missing source, compile error); carries the
    /// reason.
    #[error("custom font could not be built: {0}")]
    CustomFont(String),
    /// The entry has no bytes of its own (the typing panel's ad-hoc custom entry).
    #[error("this font entry has no bytes of its own")]
    NoBytes,
}

/// The whole byte buffer of `font`: the file's current content, the resident bundled font,
/// or a custom font's compiled full face.
///
/// BLOCKING — a file read, or for a custom font possibly a compile of several seconds:
/// worker threads only (e.g. the settings font-properties analysis).
///
/// # Errors
/// See [`FontBytesError`].
pub fn font_bytes_blocking(font: &FontEntry) -> Result<FontBytes, FontBytesError> {
    match font.byte_source() {
        FontByteSource::File(path) => std::fs::read(path)
            .map(|bytes| Arc::new(bytes) as FontBytes)
            .map_err(|err| FontBytesError::Unreadable(format!("{}: {err}", path.display()))),
        // `ms_fonts::bytes` logs its own failure with the path and the reason.
        FontByteSource::Bundled(stack_font) => ms_fonts::bytes(stack_font)
            .map(|bytes| Arc::new(bytes) as FontBytes)
            .ok_or_else(|| FontBytesError::Unreadable(stack_font.path.display().to_string())),
        FontByteSource::Custom(custom) => match custom.full_face_blocking() {
            Some(Ok(face)) => Ok(Arc::clone(&face.bytes)),
            Some(Err(err)) => Err(FontBytesError::CustomFont(err.to_string())),
            None => Err(FontBytesError::NoBytes),
        },
    }
}

/// Writes a still-pending debounced per-font-settings save immediately, on the calling
/// thread. Returns whether there was anything to flush. Called from the app's `on_exit`: a
/// parameter edit persists through a multi-second debounce, and closing the app inside that
/// window would otherwise lose it.
pub fn flush_pending_saves() -> bool {
    font_settings_store::flush_pending_saves()
}

/// Enumerates ALL OS-installed fonts — the catalog for the system-font import picker.
/// VERY HEAVY (whole OS font database); run off the GUI thread.
///
/// SIDE EFFECT: the enumeration is also published as the process-wide `PostScript name → file`
/// index used to locate an imported system font that moved, so opening the picker is what
/// refreshes that index after the user installs or removes fonts.
#[must_use]
pub fn load_system_catalog() -> Vec<FontEntry> {
    fonts::load_system_fonts()
}

/// Current revision of the font-settings store; advances on any add/remove/override/group
/// change so a cached font list can detect staleness. Cheap; may be polled from the GUI
/// thread.
#[must_use]
pub fn fonts_revision() -> u64 {
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
pub fn add_imported_font(identity: &str, path: PathBuf) -> bool {
    font_settings_store::add_imported_system_font(identity, path)
}

/// An installed system font found by name: the identity the file actually claims plus the file
/// it was read from.
///
/// `identity` is the CONFIRMED PostScript name in the casing the file declares — the key every
/// per-font setting is stored under. `path` is only the byte-source hint to record with an
/// import; it is never a key.
#[derive(Debug, Clone)]
pub struct SystemFontLocation {
    /// Confirmed PostScript name of the located font.
    pub identity: String,
    /// File the identity was read from.
    pub path: std::path::PathBuf,
}

/// Finds the font INSTALLED IN THE SYSTEM whose PostScript name is `post_script_name`.
///
/// BLOCKING and potentially VERY HEAVY: a cold lookup builds the process-wide system-font name
/// index (a scan of the whole OS font database) and reads every candidate file. Call it ONLY
/// off the GUI thread.
///
/// Returns `None` for a blank name, when no installed font declares it, and when every
/// candidate turned out not to claim it after all. When several installed files declare one
/// name, the returned file is the lowest-content-hash one (ties broken by the
/// lexicographically first path) — the same claimant the bare name resolves to everywhere else
/// in the app.
#[must_use]
pub fn locate_system_font_by_identity(post_script_name: &str) -> Option<SystemFontLocation> {
    fonts::locate_system_font_file_by_identity(post_script_name).map(|(identity, path)| {
        SystemFontLocation { identity, path }
    })
}

/// Imports SEVERAL system fonts as ONE store mutation: one revision bump and one document
/// write for the whole batch, not one per font.
///
/// `fonts` holds `(identity, path)` pairs — the font's PostScript name and the hint of where
/// its bytes were last seen — applied in the given order. An entry whose font is already
/// imported (by identity, case-insensitively, or by that exact path) is SKIPPED, and a blank
/// identity is refused with a log warning. Returns how many fonts were really added; `0` means
/// nothing changed, and then the revision is NOT bumped and nothing is persisted.
///
/// Prefer this over a loop of [`add_imported_font`] for a bulk import: every bump makes each
/// open panel reload its font list.
pub fn add_imported_fonts(fonts: &[(String, PathBuf)]) -> usize {
    font_settings_store::add_imported_system_fonts(fonts)
}

/// Adds SEVERAL fonts to virtual group `group` as ONE store mutation: one revision bump and
/// one document write for the whole batch, not one per member.
///
/// `members` holds `(identity, alias)` pairs appended in the given order, so the caller's order
/// becomes the group's member order. A font that is already a member is SKIPPED and KEEPS its
/// existing alias (a batch add never overwrites what the user set); a blank or whitespace-only
/// alias is stored as "no alias"; a blank identity is refused with a log warning. Returns how
/// many members were really added; `0` (nothing added, or no such group — which is logged)
/// leaves the revision untouched and persists nothing.
pub fn add_virtual_group_members(group: &str, members: &[(String, Option<String>)]) -> usize {
    font_settings_store::add_virtual_group_members(group, members)
}

/// Normalizes a font IDENTITY for COMPARISON and MAP KEYS: `trim` + ASCII lowercase.
///
/// THE rule every identity comparison in the app must go through — the panel's virtual-group
/// merge (`fonts::apply_virtual_groups`), the store's member lookups and the settings UI all
/// key on this, so a member persisted as `roboto-bold` and a font loaded as `Roboto-Bold` are
/// one font everywhere. Comparing identities byte-for-byte instead makes the settings UI flag
/// as missing a member the typing panel resolves without trouble.
///
/// Identities are always STORED with their original casing; only comparisons fold it, so the
/// result is a lookup key and must never be written back to a document. Pure and cheap;
/// GUI-thread safe.
#[must_use]
pub fn normalize_font_identity(name: &str) -> String {
    fonts::normalize_font_identity(name)
}

/// Whether `identity` names the synthetic BUNDLED interface-font entry (in either the current
/// or the legacy spelling, case-insensitively).
///
/// That entry is not part of [`load_font_lists`] — it is prepended to the TYPING PANEL's font
/// list instead — so a caller that decides "is this identity available?" from the font
/// categories alone must special-case it, or it reports the built-in font as missing. Pure and
/// cheap; GUI-thread safe.
#[must_use]
pub fn is_bundled_ui_font_identity(identity: &str) -> bool {
    fonts::is_reserved_bundled_identity(identity)
}

/// Whether `name` is usable as a font IDENTITY, i.e. is a spec-valid PostScript font name:
/// after trimming, 1..=63 printable ASCII characters with no space and none of the PostScript
/// delimiters `[](){}<>/%`. Pure and cheap; GUI-thread safe.
#[must_use]
pub fn is_valid_post_script_name(name: &str) -> bool {
    fonts::is_valid_post_script_name(name)
}

/// Removes a previously-imported system font by its IDENTITY. Returns `false` when it was
/// not imported. Persists off the GUI thread and bumps the store revision.
pub fn remove_imported_font(identity: &str) -> bool {
    font_settings_store::remove_imported_system_font(identity)
}

/// Whether a system font with this IDENTITY is currently imported. Cheap; GUI-thread safe.
#[must_use]
pub fn is_font_imported(identity: &str) -> bool {
    font_settings_store::is_system_font_imported(identity)
}

/// Reads the user display-name override for the font `identity`, if any.
#[must_use]
pub fn display_name_override(identity: &str) -> Option<String> {
    font_settings_store::font_display_name_override(identity)
}

/// Sets (or, with `None`, clears) the user display-name override for the font `identity`.
/// Returns whether the stored value changed. Persists off the GUI thread and bumps the
/// store revision, so cached font lists reload.
pub fn set_display_name_override(identity: &str, value: Option<String>) -> bool {
    font_settings_store::set_font_display_name_override(identity, value)
}

/// Reads the user-defined custom kerning pairs of the font `identity`, in user order (empty
/// when it has none). Offsets are in THOUSANDTHS OF AN EM. Cheap (in-memory snapshot);
/// GUI-thread safe.
#[must_use]
pub fn custom_kerning(identity: &str) -> Vec<CustomKerningPair> {
    font_settings_store::font_custom_kerning(identity)
}

/// Replaces the custom kerning pairs of the font `identity` with `pairs`, sanitized (see
/// [`CustomKerningPair`]). A pair whose offset is `0.0` is KEPT: it cancels a built-in pair.
///
/// Returns `true` when the stored list actually changed; only then does it persist off the
/// GUI thread and bump the store revision, which is what makes every open typing panel
/// reload its fonts and pick up the new pairs. Cheap and GUI-thread safe either way.
pub fn set_custom_kerning(identity: &str, pairs: Vec<CustomKerningPair>) -> bool {
    font_settings_store::set_font_custom_kerning(identity, pairs)
}

/// A virtual font group exposed to non-typing code: its name and members, with each member
/// referenced by the font's IDENTITY.
#[derive(Debug, Clone)]
pub struct VirtualFontGroupInfo {
    /// Group display name.
    pub name: String,
    /// Ordered members (user order preserved).
    pub members: Vec<VirtualFontGroupMemberInfo>,
}

/// One member of a [`VirtualFontGroupInfo`]: the referenced font's IDENTITY plus its
/// optional per-group display alias.
#[derive(Debug, Clone)]
pub struct VirtualFontGroupMemberInfo {
    /// IDENTITY of the referenced real font (`FontEntry::render_identity_name`). A member
    /// left over from an unmigrated legacy document may still hold a path-shaped string; it
    /// simply resolves to no loaded font and is shown as unavailable.
    pub font: String,
    /// Optional per-group display alias; `None` means "use the font's own label".
    pub alias: Option<String>,
}

/// Lists all virtual font groups. Cheap (in-memory snapshot); GUI-thread safe.
#[must_use]
pub fn list_virtual_groups() -> Vec<VirtualFontGroupInfo> {
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
pub fn create_virtual_group(name: &str) -> bool {
    font_settings_store::create_virtual_group(name)
}

/// Deletes the virtual group named exactly `name`. Returns `false` when none matched.
/// Persists off the GUI thread and bumps the store revision.
pub fn delete_virtual_group(name: &str) -> bool {
    font_settings_store::delete_virtual_group(name)
}

/// Renames virtual group `old` to `new`. Returns `false` when `new` is blank, `old` is
/// missing, the name is unchanged, or `new` collides case-insensitively with another group.
/// Persists off the GUI thread and bumps the store revision.
pub fn rename_virtual_group(old: &str, new: &str) -> bool {
    font_settings_store::rename_virtual_group(old, new)
}

/// Adds the font `identity` to virtual group `group`. Returns `false` when the group is
/// unknown or the font is already a member. Persists off the GUI thread and bumps the store
/// revision.
pub fn add_virtual_group_member(group: &str, identity: &str) -> bool {
    font_settings_store::add_virtual_group_member(group, identity)
}

/// Removes the font `identity` from virtual group `group`. Returns `false` when the group is
/// unknown or the font was not a member. Persists off the GUI thread and bumps the store revision.
pub fn remove_virtual_group_member(group: &str, identity: &str) -> bool {
    font_settings_store::remove_virtual_group_member(group, identity)
}

/// Sets (or, with `None`/blank, clears) the per-group display alias of the font `identity`
/// in virtual group `group`. Returns `false` when the group/member is missing or the alias
/// is unchanged. Persists off the GUI thread and bumps the store revision.
pub fn set_virtual_group_member_alias(
    group: &str,
    identity: &str,
    alias: Option<&str>,
) -> bool {
    font_settings_store::set_virtual_group_member_alias(group, identity, alias)
}

/// Returns, for the font `identity`, every virtual group that contains it as `(group name,
/// per-group alias)`. For the font properties window. Cheap (in-memory scan); GUI-thread safe.
#[must_use]
pub fn virtual_groups_for_font(identity: &str) -> Vec<(String, Option<String>)> {
    font_settings_store::virtual_groups_for_font(identity)
}

/// Lists the real FOLDER-group names discovered under `fonts/groups/`. HEAVY: performs
/// filesystem I/O (one `read_dir` of the groups directory) — callers should invoke it from
/// their existing off-thread font loads, not per frame on the GUI thread.
#[must_use]
pub fn list_folder_group_names() -> Vec<String> {
    fonts::load_font_groups(&ms_config::storage_mode::app_fonts_dir())
}

/// Why a saved custom font cannot be used by this build (facade mirror of the store's reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomFontUnsupported {
    /// Its kind is not implemented by this build (a newer build saved it); carries the tag.
    UnknownKind(String),
    /// Its recipe does not parse or breaks the recipe invariants; carries the log-ready reason.
    Invalid(String),
}

/// One saved custom font exposed to non-typing code.
///
/// Exactly one of `recipe` / `unsupported` is `Some`: a usable font carries its recipe, an
/// entry this build cannot interpret carries the reason (it is kept verbatim in the document
/// and may only be deleted).
#[derive(Debug, Clone)]
pub struct CustomFontInfo {
    /// The saved identity (`MSCustom-<16 hex>`): the key layers, groups and kerning use.
    pub identity: String,
    /// User-visible name (never blank).
    pub name: String,
    /// The recipe, when this build can use it.
    pub recipe: Option<Arc<CustomFontRecipe>>,
    /// Why the font cannot be used, when it cannot.
    pub unsupported: Option<CustomFontUnsupported>,
}

/// Lists every saved custom font in identity order, unsupported ones included. Cheap
/// (in-memory snapshot); GUI-thread safe.
#[must_use]
pub fn list_custom_fonts() -> Vec<CustomFontInfo> {
    font_settings_store::custom_fonts()
        .into_iter()
        .map(|(identity, record)| {
            let (recipe, unsupported) = match record.body {
                fonts_data::CustomFontBody::Known(recipe) => (Some(recipe), None),
                fonts_data::CustomFontBody::Unsupported { reason, .. } => (
                    None,
                    Some(match reason {
                        fonts_data::UnsupportedCustomFont::UnknownKind(kind) => CustomFontUnsupported::UnknownKind(kind),
                        fonts_data::UnsupportedCustomFont::Invalid(reason) => CustomFontUnsupported::Invalid(reason),
                        fonts_data::UnsupportedCustomFont::IdentityCollision { kept } => {
                            CustomFontUnsupported::Invalid(format!("the identity duplicates the custom font '{kept}'"))
                        }
                    }),
                ),
            };
            CustomFontInfo { identity, name: record.name, recipe, unsupported }
        })
        .collect()
}

/// The user-visible name of the saved custom font `identity` (matched case-insensitively), or
/// `None` when no custom font has it — e.g. to name a group member whose custom font is not
/// loadable this run. Cheap (in-memory read); GUI-thread safe.
#[must_use]
pub fn custom_font_name(identity: &str) -> Option<String> {
    font_settings_store::custom_font_name(identity)
}

/// Saves a new custom font named `name` with `recipe` and returns its minted saved identity.
/// Persists off the GUI thread and bumps the store revision.
///
/// # Errors
/// See [`CustomFontStoreError`]: blank or taken name (among custom fonts only — a clash with
/// a real font's name is the caller's check), invalid recipe, identity collision.
pub fn create_custom_font(name: &str, recipe: CustomFontRecipe) -> Result<String, CustomFontStoreError> {
    font_settings_store::create_custom_font(name, recipe)
}

/// Replaces the recipe of the saved custom font `identity` (its identity is unchanged). An
/// identical recipe is a no-op. Persists off the GUI thread and bumps the store revision on a
/// real change.
///
/// # Errors
/// Invalid recipe, unknown identity, or an unsupported entry (never overwritten).
pub fn update_custom_font_recipe(identity: &str, recipe: CustomFontRecipe) -> Result<(), CustomFontStoreError> {
    font_settings_store::update_custom_font_recipe(identity, recipe)
}

/// Renames the saved custom font `identity`. The unchanged name is a no-op. Persists off the
/// GUI thread and bumps the store revision on a real change.
///
/// # Errors
/// Blank or taken name, unknown identity, or an unsupported entry.
pub fn rename_custom_font(identity: &str, new_name: &str) -> Result<(), CustomFontStoreError> {
    font_settings_store::rename_custom_font(identity, new_name)
}

/// Deletes the saved custom font `identity` together with its own kerning/profile record and
/// its virtual-group memberships, in one write. Returns `false` when no such font exists.
/// Persists off the GUI thread and bumps the store revision.
pub fn delete_custom_font(identity: &str) -> bool {
    font_settings_store::delete_custom_font(identity)
}

/// Serializes every test that touches the PROCESS-GLOBAL font-settings store, across ALL
/// crates' test modules — the store is one static, so a settings-UI test and a typing-model
/// test that both reset it would otherwise race. Test-only; part of the facade so external
/// tests never reach for a typing internal.
///
/// Compiled for this crate's own tests and, through the `test-support` feature, for the
/// binary's settings-UI tests. Declaring that feature in the binary's `[dev-dependencies]`
/// only is what keeps this helper out of the shipped build.
#[cfg(any(test, feature = "test-support"))]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    font_settings_store::test_lock()
}

/// Clears the shared font-settings store to a known-empty baseline for an isolated test.
/// Callers MUST hold [`test_lock`]. Test-only; see the store's own `test_reset` for exactly
/// what is (and is not) reset.
#[cfg(any(test, feature = "test-support"))]
pub fn test_reset() {
    font_settings_store::test_reset();
}
