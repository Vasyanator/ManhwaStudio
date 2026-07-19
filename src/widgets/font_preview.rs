/*
FILE HEADER (widgets/font_preview.rs)

Purpose:
Shared egui font-registration helpers for drawing a font's own-typeface preview.
A font row that renders its name in the font itself must register that font file as
an egui font family first; this module owns the deterministic naming, the bound-check,
and the one-time file read + registration used by every such preview site.

Key functions:
- `combo_font_family_name`: deterministic egui family name for a `(path, face_index)`.
- `is_font_family_bound`: whether an egui family is already registered in a context.
- `ensure_font_family`: register the font (if needed) and return its family.

Notes:
Registration reads the font file bytes ON THE CALLING (GUI) thread on first use only,
exactly where the preview is drawn — egui needs the bytes to build the atlas. egui's
`add_font` is ADD-ONLY (no eviction), so callers that scroll large font catalogs must
bound how many distinct families they register (see the settings font-import picker).
Used by the typing create/edit panels (`create_presets::ensure_combo_font_family`) and
the settings font-settings widget; not domain-specific to either.
*/

use eframe::egui;
use std::fs;
use std::path::Path;

/// Deterministic egui family name for a UI font preview of `(font_path, face_index)`.
///
/// Depends ONLY on the path and face index, so the same file always registers under the
/// same name (safe to share across panels that share one egui `Context`) and different
/// files get different names. Sequential numbering would collide across independent
/// panels (egui stores font data by name), so a later registration would overwrite an
/// earlier one and a panel would draw the wrong font.
#[must_use]
pub fn combo_font_family_name(font_path: &Path, face_index: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_path.hash(&mut hasher);
    face_index.hash(&mut hasher);
    format!("typing-panel-combo-font-{:016x}", hasher.finish())
}

/// Whether `family` is already registered in `ctx`'s font definitions.
#[must_use]
pub fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

/// Ensures the font at `font_path` (representative `face_index`) is registered as an egui
/// font family and returns it. Reuses an existing binding (shared via the deterministic
/// `combo_font_family_name`), so a font already registered elsewhere is not read again.
/// Returns `None` when the file cannot be read or egui does not bind it. Reads the font
/// file on the CALLING (GUI) thread ON FIRST USE ONLY — registration inherently needs
/// the bytes.
#[must_use]
pub fn ensure_font_family(
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
