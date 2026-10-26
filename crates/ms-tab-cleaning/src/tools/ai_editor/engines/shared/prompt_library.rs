/*
File: cleaning/tools/ai_editor/engines/shared/prompt_library.rs

Purpose:
The model-agnostic half of a large-model engine's prompt-cache LIBRARY (protocol 23): the
shapes a `.prompt_cache.list` answer and its `EntryInfo` records parse into, and the pure rules
that tie the library to the prompt field — which preset the typed text IS (exact match after
trimming), the «Нет» stash that keeps the last prompt no preset holds, and what choosing a row
of the picker writes into the field.

Main responsibilities:
- parse `EntryInfo` (`parse_entry_info`: listing rows, `.status.prompt_cache_entry`,
  `.build.written`) and a whole listing (`parse_prompt_library_listing`), tolerantly: every field
  is optional and a row with nothing to act on is dropped;
- find the preset of the typed text (`PromptLibraryListing::find`) and derive the picker's
  selection from it (`derived_pick`) — the selection is never stored;
- keep the «Нет» stash current (`track_uncached`), apply a pick (`apply_pick`) and keep the
  preset-name field in step with the typed text (`sync_preset_name`);
- the display helpers every engine shares: a preset's default name (`default_preset_name`), a
  one-line preview (`preview_line`) and the creation date (`format_library_created`).

Key structures:
- `PromptLibraryEntry`, `PromptLibraryText`, `PromptLibraryListing`, `PromptLibraryLegacy`
- `PromptPick`

Key functions:
- `parse_entry_info()`, `parse_prompt_library_listing()`
- `derived_pick()`, `track_uncached()`, `apply_pick()`, `sync_preset_name()`
- `default_preset_name()`, `preview_line()`, `format_library_created()`

Notes:
GUI-free and model-agnostic: the method names, the request headers, the library id and the
encoder captions are the engine's. Which entry a run uses is the BACKEND's selection
(`selected` of a text row, `prompt_cache_entry` of `.status`); this side renders it and never
re-derives it. The drawing half is `prompt_library_ui.rs`.
*/

use serde_json::Value;

/// Characters a preset's default name keeps from its prompt.
const DEFAULT_NAME_CHARS: usize = 40;

/// Longest preset name the backend stores, in characters (`MAX_PRESET_NAME_CHARS` of
/// `modules/ai_backend/runtime/prompt_library.py`; a mirror, guarded by a test).
pub(in crate::tools::ai_editor::engines) const MAX_PRESET_NAME_CHARS: usize = 100;

/// One library entry — one `.msprompt` file (or a memory-only encode) — as the backend's
/// `EntryInfo` describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptLibraryEntry {
    /// The entry's file name inside the library: what `.export` names. Empty for a
    /// memory-only encode (`origin` `memory`), which has no file.
    pub(in crate::tools::ai_editor::engines) file: String,
    /// The preset name this entry carries; the text row's own `name` wins over it.
    pub(in crate::tools::ai_editor::engines) name: String,
    /// The encoder source id that produced it (`stock`, `uncensored`, …); empty when unknown
    /// (a manual-mode encoder, or a migrated entry whose source could not be proven).
    pub(in crate::tools::ai_editor::engines) encoder_source: String,
    /// The encoder's weight-format id (`bf16`, `q8_0`, …, `gguf`); empty when unknown.
    pub(in crate::tools::ai_editor::engines) encoder_format: String,
    /// Whether the encoder ran with weight-only fp8 (its precision id is then `fp8`).
    pub(in crate::tools::ai_editor::engines) encoder_fp8: bool,
    /// The backend's human label of the encoder (folder or file name), the caption of last
    /// resort when the source is unknown.
    pub(in crate::tools::ai_editor::engines) encoder_label: String,
    /// Whether it was made by the encoder the REQUEST names (fingerprint equality). `None`
    /// when the backend did not say; `Some(false)` also without any encoder.
    pub(in crate::tools::ai_editor::engines) current_encoder: Option<bool>,
    /// When it was created, already formatted ([`format_library_created`]); empty = no date.
    pub(in crate::tools::ai_editor::engines) created: String,
    pub(in crate::tools::ai_editor::engines) size_bytes: u64,
    /// `built`, `migrated_v1`, `migrated_v1_unverified`, `imported` or `memory`; an unknown
    /// literal is kept verbatim.
    pub(in crate::tools::ai_editor::engines) origin: String,
}

impl PromptLibraryEntry {
    /// The precision id the backend ranks this entry by: `fp8` when it was encoded with
    /// weight-only fp8, else its format id.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn precision_id<'a>(&'a self, fp8_id: &'a str) -> &'a str {
        if self.encoder_fp8 { fp8_id } else { self.encoder_format.as_str() }
    }

    /// Whether this is a memory-only encode the backend holds but never wrote to the library.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn is_memory_only(&self) -> bool {
        self.origin == "memory"
    }
}

/// One cached TEXT of the library: a named preset and every entry that encodes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptLibraryText {
    /// The exact text the entries encode (the backend's normalization: trimmed).
    pub(in crate::tools::ai_editor::engines) prompt: String,
    /// The preset name; empty when no entry carries one ([`Self::display_name`] then derives
    /// one from the text).
    pub(in crate::tools::ai_editor::engines) name: String,
    /// The entry a run would use for this text — the backend's selection, never re-derived.
    pub(in crate::tools::ai_editor::engines) selected: PromptLibraryEntry,
    /// Every entry of this text, as the backend listed them.
    pub(in crate::tools::ai_editor::engines) entries: Vec<PromptLibraryEntry>,
}

impl PromptLibraryText {
    /// The name the picker shows: the preset name, or the default derived from the text for a
    /// preset nobody named (a migrated entry).
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn display_name(&self) -> String {
        if self.name.trim().is_empty() { default_preset_name(&self.prompt) } else { self.name.trim().to_string() }
    }
}

/// The one-time report of the v1 migration (`legacy` of a listing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptLibraryLegacy {
    /// Old entries carried into the library (`migrated`).
    pub(in crate::tools::ai_editor::engines) migrated: u64,
    /// Old entries NOT carried over — made before the encoder fix, or not provably valid —
    /// and deleted (`deleted`).
    pub(in crate::tools::ai_editor::engines) deleted: u64,
    /// Old entries that failed to migrate for another reason and were left where they are
    /// (`not_migrated`); the backend log names each.
    pub(in crate::tools::ai_editor::engines) left_in_place: u64,
}

impl PromptLibraryLegacy {
    /// Whether the report says anything at all (all three counts zero = nothing to show).
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn is_empty(&self) -> bool {
        self.migrated == 0 && self.deleted == 0 && self.left_in_place == 0
    }
}

/// A `.prompt_cache.list` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptLibraryListing {
    /// The library id the answer describes (echo of the request's).
    pub(in crate::tools::ai_editor::engines) library: String,
    /// The library's directory, for display only.
    pub(in crate::tools::ai_editor::engines) directory: String,
    /// The cached texts, in the backend's order (newest entry first).
    pub(in crate::tools::ai_editor::engines) texts: Vec<PromptLibraryText>,
    /// How many files the backend skipped as unreadable or foreign.
    pub(in crate::tools::ai_editor::engines) skipped: usize,
    /// The backend's `text_encoder_available` for the request's paths; `None` = not reported.
    pub(in crate::tools::ai_editor::engines) text_encoder_available: Option<bool>,
    /// The v1 migration report, `None` when the backend sent none.
    pub(in crate::tools::ai_editor::engines) legacy: Option<PromptLibraryLegacy>,
}

impl PromptLibraryListing {
    /// The index of the preset whose text IS `typed`: exact equality after trimming both ends,
    /// case- and inner-whitespace-sensitive (the tokenizer sees both). `None` for an empty
    /// field or a text no preset holds.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn find(&self, typed: &str) -> Option<usize> {
        let key = typed.trim();
        if key.is_empty() {
            return None;
        }
        self.texts.iter().position(|text| text.prompt.trim() == key)
    }

    /// The preset of `typed`, see [`Self::find`].
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn text_for(&self, typed: &str) -> Option<&PromptLibraryText> {
        self.find(typed).and_then(|index| self.texts.get(index))
    }
}

/// A row of the picker: «Нет» (the last uncached text) or one preset by its listing index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum PromptPick {
    Uncached,
    Text(usize),
}

/// Reads one `EntryInfo` object. `None` only for a value that is not an object: every field
/// is optional, and a caller that needs a `file` (a listing row) checks for it itself.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn parse_entry_info(value: &Value) -> Option<PromptLibraryEntry> {
    let object = value.as_object()?;
    let text = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    Some(PromptLibraryEntry {
        file: text("file"),
        name: text("name"),
        encoder_source: text("encoder_source"),
        encoder_format: text("encoder_format"),
        encoder_fp8: object.get("encoder_fp8").and_then(Value::as_bool).unwrap_or(false),
        encoder_label: text("encoder_label"),
        current_encoder: object.get("current_encoder").and_then(Value::as_bool),
        created: format_library_created(object.get("created_at").or_else(|| object.get("created"))),
        size_bytes: object.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
        origin: text("origin"),
    })
}

/// Reads a `.prompt_cache.list` answer.
///
/// Tolerant: an absent field degrades to its empty value, an entry without a `file` is
/// dropped (nothing could export it), and a text row without a prompt or without any entry is
/// dropped (there is nothing to pick). A row without `selected` takes its first entry; a row
/// without `name` takes the selected entry's, then the first named entry's. `skipped` may be a
/// count or a list; each `legacy` count (`migrated`, `deleted`, `not_migrated`) may be a number
/// or a list.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn parse_prompt_library_listing(header: &Value) -> PromptLibraryListing {
    let texts = header
        .get("texts")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_text_row).collect())
        .unwrap_or_default();
    PromptLibraryListing {
        library: header.get("library").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        directory: header.get("directory").and_then(Value::as_str).unwrap_or_default().to_string(),
        texts,
        skipped: header.get("skipped").map_or(0, count_of).try_into().unwrap_or(usize::MAX),
        text_encoder_available: header.get("text_encoder_available").and_then(Value::as_bool),
        legacy: header.get("legacy").and_then(Value::as_object).map(|legacy| PromptLibraryLegacy {
            migrated: legacy.get("migrated").map_or(0, count_of),
            deleted: legacy.get("deleted").map_or(0, count_of),
            left_in_place: legacy.get("not_migrated").map_or(0, count_of),
        }),
    }
}

/// One text row of a listing, `None` when it has nothing to pick (see the listing parser).
fn parse_text_row(row: &Value) -> Option<PromptLibraryText> {
    let prompt = row.get("prompt").and_then(Value::as_str)?.trim().to_string();
    if prompt.is_empty() {
        return None;
    }
    let entries: Vec<PromptLibraryEntry> = row
        .get("entries")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(parse_entry_info).filter(|entry| !entry.file.is_empty()).collect())
        .unwrap_or_default();
    let selected = row
        .get("selected")
        .and_then(parse_entry_info)
        .filter(|entry| !entry.file.is_empty())
        .or_else(|| entries.first().cloned())?;
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| (!selected.name.is_empty()).then(|| selected.name.clone()))
        .or_else(|| entries.iter().find(|entry| !entry.name.is_empty()).map(|entry| entry.name.clone()))
        .unwrap_or_default();
    Some(PromptLibraryText { prompt, name, selected, entries })
}

/// A count sent either as a number or as the list it counts; anything else is zero.
fn count_of(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_array().map(|items| u64::try_from(items.len()).unwrap_or(u64::MAX)))
        .unwrap_or(0)
}

/// Which picker row the typed text selects: its preset, or «Нет» for a text no preset holds
/// (and while no listing is known). DERIVED every frame, never stored.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn derived_pick(listing: Option<&PromptLibraryListing>, typed: &str) -> PromptPick {
    listing.and_then(|listing| listing.find(typed)).map_or(PromptPick::Uncached, PromptPick::Text)
}

/// Keeps the «Нет» stash — the last prompt no preset holds — current; returns `true` when it
/// changed (a settings save is owed).
///
/// - no listing known (not loaded yet, backend down): untouched, nothing can be judged;
/// - an empty field: untouched — clearing the field to type anew must not lose the stash;
/// - the typed text is no preset: the stash becomes it, verbatim;
/// - the typed text is a preset and the STASH has become one too (built or imported since):
///   cleared, «Нет» has nothing left to bring back;
/// - otherwise untouched.
pub(in crate::tools::ai_editor::engines) fn track_uncached(stash: &mut String, listing: Option<&PromptLibraryListing>, typed: &str) -> bool {
    let Some(listing) = listing else {
        return false;
    };
    if typed.trim().is_empty() {
        return false;
    }
    let next = if listing.find(typed).is_none() {
        typed
    } else if listing.find(stash).is_some() {
        ""
    } else {
        return false;
    };
    if stash.as_str() == next {
        return false;
    }
    next.clone_into(stash);
    true
}

/// Applies a picker row to the prompt field; returns `true` when the field changed.
///
/// A preset writes its exact text; «Нет» writes the stash back, and does nothing while the
/// stash is empty (a blank prompt would block the run). Picking the row the field already
/// shows changes nothing.
pub(in crate::tools::ai_editor::engines) fn apply_pick(pick: PromptPick, listing: Option<&PromptLibraryListing>, stash: &str, prompt: &mut String) -> bool {
    let target = match pick {
        PromptPick::Text(index) => match listing.and_then(|listing| listing.texts.get(index)) {
            Some(text) => text.prompt.as_str(),
            None => return false,
        },
        PromptPick::Uncached => {
            if stash.trim().is_empty() {
                return false;
            }
            stash
        }
    };
    if prompt.as_str() == target {
        return false;
    }
    target.clone_into(prompt);
    true
}

/// THE preset-name normalization table: inclusive code-point ranges a stored name replaces by
/// a space — every control character (`Cc`), the format characters in practical use (soft
/// hyphen, Arabic / Syriac marks, Mongolian vowel separator, zero-width and bidi controls,
/// invisible operators, BOM, interlinear annotation, the tag block) and the private-use areas.
/// A mirror of `NAME_REMOVED_RANGES` in `modules/ai_backend/runtime/prompt_library.py`: both
/// sides' tests compare their table and their output with
/// `modules/ai_backend/runtime/fixtures/preset_name_normalization.json`, so a drift fails a test.
const NAME_REMOVED_RANGES: [(u32, u32); 18] = [
    (0x0000, 0x001F),
    (0x007F, 0x009F),
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xE000, 0xF8FF),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xF0000, 0x10FFFF),
];

/// A preset name as the backend STORES it (`normalize_preset_name` there, the same rule):
/// every [`NAME_REMOVED_RANGES`] character becomes a space, the ends are trimmed, at most
/// [`MAX_PRESET_NAME_CHARS`] characters (not bytes) are kept and the cut end is trimmed again.
/// Comparing a typed name with a stored one through this keeps a name the backend would
/// shorten or clean from looking like a pending rename forever. `str::trim` and Python's
/// `str.strip` differ only on U+001C..U+001F, which the table replaces first.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn normalize_preset_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if is_name_removed_char(c) { ' ' } else { c }).collect();
    let capped: String = cleaned.trim().chars().take(MAX_PRESET_NAME_CHARS).collect();
    capped.trim().to_string()
}

/// Whether `c` is in [`NAME_REMOVED_RANGES`] (replaced by a space in a stored name).
fn is_name_removed_char(c: char) -> bool {
    let code = u32::from(c);
    NAME_REMOVED_RANGES.iter().any(|&(low, high)| (low..=high).contains(&code))
}

/// Keeps the preset NAME field in step with the typed text; returns `true` when it rewrote it.
///
/// The field follows the preset the text IS (`hit`): when that changes — another preset, a
/// text no preset holds, or the first listing — the field takes the preset's name, or is
/// cleared for an uncached text. `synced_for` remembers which preset text the field was last
/// filled for, so the user's own edit of the name survives every frame in which the preset
/// stays the same (that edit is what «Переименовать» sends).
pub(in crate::tools::ai_editor::engines) fn sync_preset_name(name: &mut String, synced_for: &mut Option<String>, hit: Option<&PromptLibraryText>) -> bool {
    let key = hit.map(|text| text.prompt.as_str());
    if synced_for.as_deref() == key {
        return false;
    }
    *synced_for = key.map(str::to_string);
    *name = hit.map(PromptLibraryText::display_name).unwrap_or_default();
    true
}

/// `text` on one line (every whitespace run collapsed to one space), cut to `max_chars`
/// characters with a trailing ellipsis when longer.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn preview_line(text: &str, max_chars: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let line = words.join(" ");
    if line.chars().count() <= max_chars {
        return line;
    }
    let mut cut: String = line.chars().take(max_chars).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// A preset name for DISPLAY when a row carries none: the first [`DEFAULT_NAME_CHARS`]
/// characters of its prompt on one line. The STORED default of an unnamed preset is the
/// backend's (`runtime.prompt_library.default_preset_name`, the same rule); this only covers a
/// row that reached the client without a name.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn default_preset_name(prompt: &str) -> String {
    preview_line(prompt, DEFAULT_NAME_CHARS)
}

/// Formats an entry's creation time for display.
///
/// The backend writes an ISO-8601 UTC string (`created_at`), shown verbatim; a Unix timestamp
/// in seconds (integer or float) is accepted too and rendered in the machine's local time.
/// Anything else — and a timestamp outside the representable range — is an empty string, shown
/// as "no date" rather than as a wrong one.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn format_library_created(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| {
                // Python's `time.time()` is a FLOAT. Cast justification: the guard keeps the
                // value finite and inside ±1e15 s, far below `i64`'s range, so the truncation
                // neither wraps nor loses the whole-second part; `f64` has no fallible
                // conversion to `i64`.
                let seconds = number.as_f64()?;
                (seconds.is_finite() && seconds.abs() < 1e15).then(|| seconds.trunc() as i64)
            })
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|utc| utc.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default(),
        Some(_) | None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(file: &str, source: &str, format: &str) -> Value {
        json!({ "file": file, "encoder_source": source, "encoder_format": format, "created_at": "2026-10-26T10:00:00Z" })
    }

    fn listing(texts: &[&str]) -> PromptLibraryListing {
        parse_prompt_library_listing(&json!({
            "library": "flux2_klein_9b",
            "texts": texts.iter().enumerate().map(|(index, text)| json!({
                "prompt": text,
                "name": format!("preset {index}"),
                "selected": entry(&format!("{index}.msprompt"), "stock", "bf16"),
                "entries": [entry(&format!("{index}.msprompt"), "stock", "bf16")],
            })).collect::<Vec<_>>(),
        }))
    }

    #[test]
    fn a_listing_parses_tolerantly_and_drops_rows_with_nothing_to_pick() {
        let parsed = parse_prompt_library_listing(&json!({
            "library": "flux2_klein_9b",
            "directory": "/root/prompt_cache/flux2_klein_9b",
            "skipped": [{ "file": "broken.msprompt", "reason": "corrupt" }],
            "text_encoder_available": false,
            "legacy": { "migrated": 1, "deleted": [ "a", "b" ], "directory": "/root/prompt_cache" },
            "texts": [
                {
                    "prompt": "  remove the sfx  ",
                    "selected": { "file": "a.msprompt", "name": "sfx", "encoder_source": "uncensored", "encoder_format": "q8_0",
                                  "encoder_fp8": false, "current_encoder": true, "created_at": "2026-10-26T10:00:00Z",
                                  "size_bytes": 12, "origin": "built", "unknown_field": 7 },
                    "entries": [
                        { "file": "a.msprompt", "name": "sfx", "origin": "built" },
                        { "name": "no file" },
                        { "file": "b.msprompt", "origin": "something_new" }
                    ]
                },
                { "prompt": "", "entries": [{ "file": "c.msprompt" }] },
                { "prompt": "nothing listed" },
                { "prompt": "no selected", "entries": [{ "file": "d.msprompt", "name": "named" }] },
                { "entries": [{ "file": "e.msprompt" }] }
            ]
        }));
        assert_eq!(parsed.library, "flux2_klein_9b");
        assert_eq!(parsed.skipped, 1);
        assert_eq!(parsed.text_encoder_available, Some(false));
        assert_eq!(parsed.legacy, Some(PromptLibraryLegacy { migrated: 1, deleted: 2, left_in_place: 0 }));
        let left = parse_prompt_library_listing(&json!({ "legacy": { "migrated": 0, "deleted": 0, "not_migrated": 3 } }));
        assert_eq!(left.legacy, Some(PromptLibraryLegacy { migrated: 0, deleted: 0, left_in_place: 3 }));
        assert_eq!(parsed.texts.len(), 2, "rows without a prompt or without any entry are dropped");
        let first = &parsed.texts[0];
        assert_eq!(first.prompt, "remove the sfx");
        assert_eq!(first.name, "sfx", "the selected entry's name stands in for the row's");
        assert_eq!(first.selected.encoder_source, "uncensored");
        assert_eq!(first.selected.current_encoder, Some(true));
        assert_eq!(first.selected.size_bytes, 12);
        assert_eq!(first.entries.len(), 2, "an entry without a file is dropped");
        assert_eq!(first.entries[1].origin, "something_new", "an unknown origin is kept verbatim");
        assert_eq!(first.entries[1].current_encoder, None, "an absent flag is not known, never false");
        let second = &parsed.texts[1];
        assert_eq!(second.selected.file, "d.msprompt", "no `selected`: the first entry");
        assert_eq!(second.name, "named");
        assert_eq!(parse_prompt_library_listing(&json!({})), PromptLibraryListing::default());
        assert_eq!(parse_entry_info(&json!("not an object")), None);
    }

    #[test]
    fn find_is_exact_after_trimming_and_sensitive_to_case_and_inner_whitespace() {
        let library = listing(&["remove the sfx", "Clean BG"]);
        assert_eq!(library.find("remove the sfx"), Some(0));
        assert_eq!(library.find("  remove the sfx \n"), Some(0));
        assert_eq!(library.find("Remove the sfx"), None);
        assert_eq!(library.find("remove  the sfx"), None);
        assert_eq!(library.find("clean bg"), None);
        assert_eq!(library.find("Clean BG"), Some(1));
        assert_eq!(library.find("   "), None);
        assert_eq!(derived_pick(Some(&library), "Clean BG"), PromptPick::Text(1));
        assert_eq!(derived_pick(Some(&library), "typed"), PromptPick::Uncached);
        assert_eq!(derived_pick(None, "Clean BG"), PromptPick::Uncached);
    }

    #[test]
    fn the_uncached_stash_follows_the_typed_text_and_the_picks() {
        let mut library = listing(&["cached"]);
        let mut stash = String::new();
        let mut prompt = "my new text".to_string();

        // Typing a text no preset holds stashes it verbatim.
        assert!(track_uncached(&mut stash, Some(&library), &prompt));
        assert_eq!(stash, "my new text");
        assert!(!track_uncached(&mut stash, Some(&library), &prompt), "no change, no save");

        // Picking a preset replaces the prompt and keeps the stash.
        assert!(apply_pick(PromptPick::Text(0), Some(&library), &stash, &mut prompt));
        assert_eq!(prompt, "cached");
        assert!(!track_uncached(&mut stash, Some(&library), &prompt));
        assert_eq!(stash, "my new text");

        // «Нет» brings the stashed text back.
        assert!(apply_pick(PromptPick::Uncached, Some(&library), &stash, &mut prompt));
        assert_eq!(prompt, "my new text");
        assert!(!apply_pick(PromptPick::Uncached, Some(&library), &stash, &mut prompt), "already shown");

        // An unknown listing leaves the stash alone, and so does an empty field.
        assert!(!track_uncached(&mut stash, None, "something else"));
        assert!(!track_uncached(&mut stash, Some(&library), "   "));
        assert_eq!(stash, "my new text");

        // The stash text becomes a preset (built since): with a preset in the field it is cleared.
        library = listing(&["cached", "my new text"]);
        assert!(track_uncached(&mut stash, Some(&library), "my new text"), "clearing owes a save");
        assert!(stash.is_empty(), "the stash is a preset now");
        // «Нет» with an empty stash writes nothing (a blank prompt would block the run).
        assert!(!apply_pick(PromptPick::Uncached, Some(&library), &stash, &mut prompt));
        assert_eq!(prompt, "my new text");
        // A pick past the end of the listing changes nothing.
        assert!(!apply_pick(PromptPick::Text(9), Some(&library), &stash, &mut prompt));
    }

    #[test]
    fn the_name_field_follows_the_preset_and_keeps_the_users_edit() {
        let library = listing(&["cached", "other"]);
        let mut name = String::new();
        let mut synced_for = None;
        // The first listing fills the field with the typed preset's name.
        assert!(sync_preset_name(&mut name, &mut synced_for, library.text_for("cached")));
        assert_eq!(name, "preset 0");
        // The user edits the name: the same preset keeps the edit.
        name = "renamed".to_string();
        assert!(!sync_preset_name(&mut name, &mut synced_for, library.text_for("cached")));
        assert_eq!(name, "renamed");
        // Another preset takes its own name; an uncached text clears the field once…
        assert!(sync_preset_name(&mut name, &mut synced_for, library.text_for("other")));
        assert_eq!(name, "preset 1");
        assert!(sync_preset_name(&mut name, &mut synced_for, library.text_for("typed")));
        assert!(name.is_empty());
        // …and a name typed for that new text survives further typing.
        name = "my name".to_string();
        assert!(!sync_preset_name(&mut name, &mut synced_for, library.text_for("typed more")));
        assert_eq!(name, "my name");
    }

    #[test]
    fn a_preset_name_is_normalized_like_the_backend_stores_it() {
        assert_eq!(normalize_preset_name("  sfx\tremoval\u{200B}x  "), "sfx removal x");
        let long = "я".repeat(MAX_PRESET_NAME_CHARS + 20);
        let capped = normalize_preset_name(&long);
        assert_eq!(capped.chars().count(), MAX_PRESET_NAME_CHARS, "counted in chars, not bytes");
        assert_eq!(normalize_preset_name(&format!("{} tail", "a".repeat(MAX_PRESET_NAME_CHARS - 1))), "a".repeat(MAX_PRESET_NAME_CHARS - 1), "the cut end is trimmed");
        assert_eq!(normalize_preset_name("\n\u{FEFF} "), "");
    }

    /// The fixture both implementations are tested against (the Python test reads it too).
    fn name_fixture() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/ai_backend/runtime/fixtures/preset_name_normalization.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

    #[test]
    fn the_name_table_and_cap_are_the_shared_fixture_s() {
        let fixture = name_fixture();
        let ranges: Vec<(u32, u32)> = fixture["removed_ranges"]
            .as_array()
            .expect("removed_ranges is an array")
            .iter()
            .map(|pair| {
                let bound = |index: usize| pair[index].as_u64().and_then(|value| u32::try_from(value).ok()).expect("a range bound is a u32");
                (bound(0), bound(1))
            })
            .collect();
        assert_eq!(ranges, NAME_REMOVED_RANGES.to_vec(), "the Rust table drifted from the shared fixture");
        assert_eq!(fixture["max_chars"].as_u64(), u64::try_from(MAX_PRESET_NAME_CHARS).ok(), "the cap drifted from the shared fixture");
        // Exhaustive over every scalar value: the classification is the table and nothing else.
        let mismatch = (0..=0x0010_FFFF_u32).filter_map(char::from_u32).find(|&c| is_name_removed_char(c) != ranges.iter().any(|&(low, high)| (low..=high).contains(&u32::from(c))));
        assert_eq!(mismatch, None);
    }

    #[test]
    fn every_shared_fixture_case_normalizes_like_the_backend() {
        let fixture = name_fixture();
        let cases = fixture["cases"].as_array().expect("cases is an array");
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["input"].as_str().expect("a case has an input");
            let expected = case["expected"].as_str().expect("a case has an expected value");
            assert_eq!(normalize_preset_name(input), expected, "{}", case["why"]);
        }
    }

    #[test]
    fn a_default_name_is_the_prompt_on_one_line_cut_to_its_budget() {
        assert_eq!(default_preset_name("  remove\nthe   sfx "), "remove the sfx");
        let long = "a".repeat(60);
        let name = default_preset_name(&long);
        assert_eq!(name.chars().count(), DEFAULT_NAME_CHARS + 1);
        assert!(name.ends_with('…'));
        let text = PromptLibraryText { prompt: "clean the background".to_string(), ..PromptLibraryText::default() };
        assert_eq!(text.display_name(), "clean the background");
    }

    #[test]
    fn a_created_timestamp_is_read_in_every_shape_the_backend_may_send() {
        assert_eq!(format_library_created(Some(&json!("  2024-05-01 12:00  "))), "2024-05-01 12:00");
        assert!(!format_library_created(Some(&json!(1_700_000_000u64))).is_empty());
        assert_eq!(
            format_library_created(Some(&json!(1_700_000_000.75f64))),
            format_library_created(Some(&json!(1_700_000_000u64))),
            "the fractional second must not change the rendered minute"
        );
        assert!(format_library_created(None).is_empty());
        assert!(format_library_created(Some(&Value::Null)).is_empty());
        assert!(format_library_created(Some(&json!(1e300))).is_empty());
    }

    #[test]
    fn the_precision_id_of_an_fp8_entry_is_fp8() {
        let plain = PromptLibraryEntry { encoder_format: "bf16".to_string(), ..PromptLibraryEntry::default() };
        assert_eq!(plain.precision_id("fp8"), "bf16");
        let fp8 = PromptLibraryEntry { encoder_fp8: true, ..plain };
        assert_eq!(fp8.precision_id("fp8"), "fp8");
    }
}
