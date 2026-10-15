/*
File: crates/ms-i18n/tests/binding_text.rs

Purpose:
Guard the input-unification rule "hint text never spells a binding": every binding a user can
rebind is displayed from the input registry (`ms_input::InputRegistry::label`, alternatives
joined by " / "), so a locale value that spells one ("Ctrl+wheel", "LMB", "(T)", "1-6") goes
stale the moment the user rebinds it.

Checks, over `en.json` and `ru.json` and the namespaces of the migrated input
(`MIGRATED_NAMESPACES`: the Translation / Cleaning / Typing tabs, their canvas and tools, the
session hotkeys, the Hotkeys settings pane and the Typing rotation chooser):
- no value (plural forms included) contains a binding token (`spelled_binding`);
- no key ends in `_keys` (the old "binding text in the catalog" naming).

Notes:
- `input.*` is EXCLUDED by design: it holds the button and gesture NAMES the label module
  composes labels from ("Ctrl", "LMB", "Wheel"), i.e. it is the one legitimate place for them.
- Namespaces of features not migrated to `ms-input` (PS editor, page manager, launcher) are out
  of scope until those features declare their actions.
- `WHITELIST` holds values that contain a token without naming a binding (an English verb or
  noun that happens to be a key name). Every entry carries its reason, and an entry that no
  longer matches anything fails the test, so the list cannot rot.
*/

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Key prefixes whose values must not spell a binding.
const MIGRATED_NAMESPACES: &[&str] = &[
    "canvas.",
    "translation.",
    "cleaning.",
    "typing.",
    "tools.",
    "app.hotkey.",
    "settings.hotkeys.",
    "settings.typesetting.rotation_",
];

/// Keys whose values contain a token that is NOT a binding, with the reason.
const WHITELIST: &[(&str, &str)] = &[
    // "Alt version" = the alternative raw scan of a page (stamp source), not the Alt key.
    ("cleaning.tools.stamp.mode_alt_version", "alt = alternative version"),
    // "(W)" is the watermark model's imprint colour symbol W shown under its preview.
    ("cleaning.tools.watermark.chapter.preview_imprint_label", "W = imprint colour symbol"),
    // "Shift X / Y" = the formula path's offset along an axis.
    ("typing.advanced.formula_offset_x_label", "shift = offset"),
    ("typing.advanced.formula_offset_y_label", "shift = offset"),
    // "Shift of the text along the line normal" = offset.
    ("typing.advanced.spacing_offset_tooltip", "shift = offset"),
    // The deform tool's "Shift" (push) mode name.
    ("typing.deform.mode_shift", "shift = push deform mode"),
    // "Shift following characters" = move them along the line.
    ("typing.params.inline_shift_following", "shift = move"),
    // The composition separator value "space" names the whitespace character.
    ("translation.composition.space_value", "space = whitespace character"),
    // "Удалять лишние пробелы" = collapse extra whitespace characters.
    ("typing.params.strip_extra_spaces", "пробелы = whitespace characters"),
];

/// Tokens matched as whole words, case-sensitively (key and mouse-button names).
const WORD_TOKENS: &[&str] = &["Ctrl", "Shift", "Alt", "Cmd", "Esc", "Del", "Space", "LMB", "RMB", "MMB"];

/// Substrings matched case-insensitively (Russian button names, the wheel in both languages).
const SUBSTRING_TOKENS: &[&str] = &["⌘", "лкм", "пкм", "скм", "пробел", "колес", "колёс", "wheel"];

/// Gesture words that, written after a "+", spell a gesture binding ("Space+drag").
const PLUS_GESTURE_WORDS: &[&str] = &["drag", "click", "перетаск", "клик"];

/// Whether `text[start..end]` is a whole word (no alphanumeric character on either side).
fn is_whole_word(text: &str, start: usize, end: usize) -> bool {
    let before = text.get(..start).and_then(|head| head.chars().next_back());
    let after = text.get(end..).and_then(|tail| tail.chars().next());
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

/// The first binding token `value` spells, or `None`.
fn spelled_binding(value: &str) -> Option<String> {
    for token in WORD_TOKENS {
        for (start, _) in value.match_indices(token) {
            if is_whole_word(value, start, start + token.len()) {
                return Some((*token).to_owned());
            }
        }
    }
    let lower = value.to_lowercase();
    if let Some(token) = SUBSTRING_TOKENS.iter().find(|token| lower.contains(*token)) {
        return Some((*token).to_owned());
    }
    // "+drag", "+ click", ...
    for (plus, _) in lower.match_indices('+') {
        let rest = lower.get(plus + 1..).unwrap_or_default().trim_start();
        if let Some(word) = PLUS_GESTURE_WORDS.iter().find(|word| rest.starts_with(*word)) {
            return Some(format!("+{word}"));
        }
    }
    let chars: Vec<(usize, char)> = value.char_indices().collect();
    for window in chars.windows(3) {
        let [(start, first), (_, middle), (last_at, last)] = window else { continue };
        let end = last_at + last.len_utf8();
        // A single key in parentheses: "(T)", "(5)".
        if *first == '(' && *last == ')' && (middle.is_ascii_uppercase() || middle.is_ascii_digit()) {
            return value.get(*start..end).map(str::to_owned);
        }
        // A key range: "1-6", not inside a longer number ("10-12").
        if first.is_ascii_digit() && *middle == '-' && last.is_ascii_digit() && is_whole_word(value, *start, end) {
            return value.get(*start..end).map(str::to_owned);
        }
    }
    None
}

/// Every `(key, value)` of a locale file, plural objects flattened to one entry per form.
fn entries(locale: &str) -> Vec<(String, String)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("locales").join(format!("{locale}.json"));
    let source = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let root: serde_json::Value = serde_json::from_str(&source).unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
    let object = root.as_object().unwrap_or_else(|| panic!("{} root is not a JSON object", path.display()));
    let mut out = Vec::new();
    for (key, value) in object {
        if key == "_meta" {
            continue;
        }
        match value {
            serde_json::Value::String(text) => out.push((key.clone(), text.clone())),
            serde_json::Value::Object(forms) => {
                out.extend(forms.values().filter_map(serde_json::Value::as_str).map(|text| (key.clone(), text.to_owned())));
            }
            _ => {}
        }
    }
    out
}

fn in_migrated_namespace(key: &str) -> bool {
    MIGRATED_NAMESPACES.iter().any(|prefix| key.starts_with(prefix))
}

/// No value under a migrated namespace spells a binding, no key there ends in `_keys`, and
/// every whitelist entry still matches.
#[test]
fn migrated_namespaces_spell_no_binding() {
    let whitelisted: BTreeSet<&str> = WHITELIST.iter().map(|(key, _)| *key).collect();
    let mut violations = Vec::new();
    let mut whitelist_used = BTreeSet::new();
    for locale in ["en", "ru"] {
        for (key, value) in entries(locale) {
            if !in_migrated_namespace(&key) {
                continue;
            }
            if key.ends_with("_keys") {
                violations.push(format!("{locale}: {key} ends in `_keys`: bindings come from the input registry"));
            }
            if let Some(token) = spelled_binding(&value) {
                if whitelisted.contains(key.as_str()) {
                    whitelist_used.insert(key.clone());
                } else {
                    violations.push(format!("{locale}: {key} spells `{token}` in {value:?}"));
                }
            }
        }
    }
    for key in &whitelisted {
        if !whitelist_used.contains(*key) {
            violations.push(format!("whitelist entry {key} matches nothing any more: remove it"));
        }
    }
    assert!(
        violations.is_empty(),
        "hint text must name bindings through `InputRegistry::label` placeholders, never spell them:\n{}",
        violations.join("\n")
    );
}

/// The matcher itself: what counts as a spelled binding and what does not.
#[test]
fn spelled_binding_detects_tokens() {
    for (text, token) in [
        ("Ctrl+wheel rotation:", "Ctrl"),
        ("Hold Shift", "Shift"),
        ("Поворот колесом", "колес"),
        ("Зажмите ЛКМ", "лкм"),
        ("{pan}+drag: pan", "+drag"),
        ("{pan} + перетаскивание", "+перетаск"),
        ("Create bubble (T)", "(T)"),
        ("Recent character 1-6", "1-6"),
        ("Mouse wheel", "wheel"),
        ("⌘", "⌘"),
    ] {
        assert_eq!(spelled_binding(text).as_deref(), Some(token), "{text}");
    }
    for text in ["Shifted", "Alternative", "Delete the bubble", "Spacer", "{binding}: rotate", "10-12 px", "Hold {pan} and drag", "(t)", "A + B"] {
        assert_eq!(spelled_binding(text), None, "{text}");
    }
}
