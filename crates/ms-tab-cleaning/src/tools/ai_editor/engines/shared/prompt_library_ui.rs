/*
File: cleaning/tools/ai_editor/engines/shared/prompt_library_ui.rs

Purpose:
The drawing half of the shared prompt-cache library: the PRESET PICKER. A
`SearchableComboBox` (a searchable catalog) whose row 0 is «Нет» — the last prompt no preset
holds — and whose other rows are the library's named presets; its selection is DERIVED from
the prompt field every frame, and choosing a row is returned as a `PromptPick` for the engine
to apply (`apply_pick`).

Key functions:
- `draw_prompt_library_picker()`

Notes:
Draws its own label (the widget draws none). Every text is a `cleaning.tools.prompt_library.*`
key or the caller's already-localized encoder caption; nothing here names a model. The rules
(which row the text selects, what a pick writes) are `prompt_library.rs`.
*/

use super::prompt_library::{PromptLibraryEntry, PromptLibraryListing, PromptPick, derived_pick, preview_line};
use eframe::egui;
use ms_widgets::{SearchableComboBox, SearchableComboItem};

/// Characters of the stash shown on the «Нет» row's second line.
const STASH_PREVIEW_CHARS: usize = 60;

/// Draws the labelled preset picker and returns the row the user chose this frame.
///
/// `id_salt` is a stable literal (the widget's state lives under it). `listing` is the last
/// `.prompt_cache.list` answer (`None` while unknown: only «Нет» is offered), `typed` the prompt
/// field, `stash` the «Нет» text and `caption` the engine's localized name of an entry's
/// encoder. The selection shown is [`derived_pick`] of `typed`; a click, `Enter` or a wheel step
/// on the closed combo answers `Some`, also for the row already shown (applying it is then a
/// no-op). Rows: primary = preset name; secondary = encoder of the entry a run would use and its
/// date; hover = the full prompt and every entry's encoder.
pub(in crate::tools::ai_editor::engines) fn draw_prompt_library_picker(
    ui: &mut egui::Ui,
    id_salt: &str,
    listing: Option<&PromptLibraryListing>,
    typed: &str,
    stash: &str,
    caption: &dyn Fn(&PromptLibraryEntry) -> String,
) -> Option<PromptPick> {
    ui.label(t!("cleaning.tools.prompt_library.picker_label"));

    // Owned texts first: the combo rows borrow them for this frame.
    let none_primary = t!("cleaning.tools.prompt_library.none_row");
    let (none_secondary, none_tooltip) = if stash.trim().is_empty() {
        (None, t!("cleaning.tools.prompt_library.none_empty_tooltip").to_string())
    } else {
        (
            Some(tf!("cleaning.tools.prompt_library.none_secondary", text = preview_line(stash, STASH_PREVIEW_CHARS))),
            tf!("cleaning.tools.prompt_library.none_tooltip", text = stash.trim()),
        )
    };
    let rows: Vec<(String, String, String)> = listing
        .map(|listing| listing.texts.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|text| {
            let encoders: Vec<String> = text.entries.iter().map(|entry| entry_line(entry, caption)).collect();
            (
                text.display_name(),
                entry_line(&text.selected, caption),
                tf!("cleaning.tools.prompt_library.row_tooltip", prompt = text.prompt, encoders = encoders.join("\n")),
            )
        })
        .collect();

    let mut items = Vec::with_capacity(rows.len() + 1);
    let none_item = match none_secondary.as_deref() {
        Some(secondary) => SearchableComboItem::with_secondary(none_primary, secondary),
        None => SearchableComboItem::new(none_primary),
    };
    items.push(none_item.tooltip(&none_tooltip));
    for (name, secondary, tooltip) in &rows {
        items.push(SearchableComboItem::with_secondary(name, secondary).tooltip(tooltip));
    }

    // Row 0 is «Нет», so preset `i` is row `i + 1`.
    let mut selected = match derived_pick(listing, typed) {
        PromptPick::Uncached => 0,
        PromptPick::Text(index) => index.saturating_add(1),
    };
    let response = SearchableComboBox::new(id_salt).width(ui.available_width()).show(ui, &mut selected, &items);
    let chosen = response.picked.or_else(|| response.changed.then_some(selected))?;
    Some(match chosen.checked_sub(1) {
        None => PromptPick::Uncached,
        Some(index) => PromptPick::Text(index),
    })
}

/// One entry as a line: its encoder caption and creation date (either may be missing).
fn entry_line(entry: &PromptLibraryEntry, caption: &dyn Fn(&PromptLibraryEntry) -> String) -> String {
    let encoder = caption(entry);
    match (encoder.is_empty(), entry.created.is_empty()) {
        (false, false) => tf!("cleaning.tools.prompt_library.row_secondary", encoder = encoder, created = entry.created),
        (false, true) => encoder,
        (true, _) => entry.created.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// The picker's texts are a catalog surface with no other explanation, so both maintained
    /// catalogs carry every key with the placeholders the code fills.
    #[test]
    fn the_reference_catalogs_carry_the_picker_keys_and_their_placeholders() {
        const KEYS: [(&str, &[&str]); 7] = [
            ("cleaning.tools.prompt_library.picker_label", &[]),
            ("cleaning.tools.prompt_library.none_row", &[]),
            ("cleaning.tools.prompt_library.none_secondary", &["{text}"]),
            ("cleaning.tools.prompt_library.none_tooltip", &["{text}"]),
            ("cleaning.tools.prompt_library.none_empty_tooltip", &[]),
            ("cleaning.tools.prompt_library.row_secondary", &["{encoder}", "{created}"]),
            ("cleaning.tools.prompt_library.row_tooltip", &["{prompt}", "{encoders}"]),
        ];
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            for (key, placeholders) in KEYS {
                let value = catalog.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks `{key}`"));
                assert!(!value.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                for placeholder in placeholders {
                    assert!(value.contains(placeholder), "locale `{tag}`: `{key}` drops `{placeholder}`");
                }
            }
        }
    }
}
