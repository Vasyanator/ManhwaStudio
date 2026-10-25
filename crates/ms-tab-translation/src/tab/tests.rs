//! Tests of the footer character data (`tab/characters.rs`): footer tracking recompute, the
//! chapter-typed names, the autocomplete base order and its reassembly on roster reloads.

use super::{Map, TranslationTabState, Value, footer_additional_character_names};
use super::characters::{
    build_translation_character_names, collect_chapter_character_names, footer_autocomplete_candidates,
    footer_tracking_should_recompute,
};
use crate::panels::bubbles::{footer_no_character, footer_no_characters};
use ms_project::Bubble;

#[test]
fn footer_tracking_not_bootstrapped_always_recomputes() {
    // Before bootstrap the first frame must recompute regardless of the cached revision.
    assert!(footer_tracking_should_recompute(None, &7, false));
    assert!(footer_tracking_should_recompute(Some(&7), &7, false));
}

#[test]
fn footer_tracking_changed_revision_recomputes() {
    // A new bubble bumps `hook_bubbles_revision` (a new `project.bubbles` allocation changes
    // the key too), so a differing key forces a recompute.
    assert!(footer_tracking_should_recompute(Some(&7), &8, true));
    assert!(footer_tracking_should_recompute(None, &8, true));
}

#[test]
fn footer_tracking_equal_revision_when_bootstrapped_skips() {
    // Unchanged bubble set on a bootstrapped tab: skip the per-frame recompute.
    assert!(!footer_tracking_should_recompute(Some(&8), &8, true));
}

/// Builds a bubble carrying `character_name` in `extra`, the way the footer stores it.
/// `class` is the persisted `bubble_class` token (`None` = a legacy text bubble).
fn character_bubble(id: i64, class: Option<&str>, character_name: &str) -> Bubble {
    let mut extra = Map::new();
    extra.insert(
        "character_name".to_string(),
        Value::String(character_name.to_string()),
    );
    Bubble {
        id,
        img_idx: 0,
        img_u: 0.5,
        img_v: 0.5,
        side: None,
        bubble_class: class.map(str::to_string),
        bubble_type: None,
        text: String::new(),
        original_text: String::new(),
        extra,
    }
}

/// Order is the suggestion priority the user sees, so it must follow the bubbles' own order
/// (ascending id) whatever order the snapshot arrives in, and a name re-used with different
/// capitalization must not produce a second entry.
#[test]
fn chapter_character_names_follow_bubble_id_order_and_dedupe_by_case() {
    let bubbles = vec![
        character_bubble(30, Some("text"), "Борис"),
        character_bubble(10, None, "Аня"),
        character_bubble(20, Some("text"), "аня"),
        character_bubble(40, Some("text"), "  Борис  "),
    ];
    assert_eq!(
        collect_chapter_character_names(&bubbles),
        vec!["Аня".to_string(), "Борис".to_string()],
        "first spelling wins and each name appears once"
    );
}

/// `footer_no_character()` genuinely reaches `Bubble.extra` (the panel's "И.П." checkbox
/// writes it as a fallback value), so filtering it here is required rather than defensive; an
/// empty field is not a name; and neither an image nor a hint bubble has a character field at
/// all — a `character_name` left in their `extra` by a class switch must not leak into the
/// base.
#[test]
fn chapter_character_names_drop_sentinels_blanks_and_classes_without_a_character() {
    let bubbles = vec![
        character_bubble(1, Some("text"), ""),
        character_bubble(2, Some("text"), "   "),
        character_bubble(3, Some("text"), footer_no_character()),
        character_bubble(4, Some("text"), footer_no_characters()),
        character_bubble(5, Some("image"), "Обложка"),
        character_bubble(6, Some("hint"), "Примечание"),
        character_bubble(7, Some("text"), "Секретарша"),
    ];
    assert_eq!(
        collect_chapter_character_names(&bubbles),
        vec!["Секретарша".to_string()],
    );
}

/// The assembled base is what the footer widget ranks its suggestions by, so the four groups
/// must stay in this exact order: sentinel, project roster, chapter-typed names, built-ins.
#[test]
fn the_autocomplete_base_orders_sentinel_roster_chapter_then_builtins() {
    let project = vec!["Аня".to_string()];
    let chapter = vec!["Секретарша".to_string()];
    let out = build_translation_character_names(&project, &chapter);
    assert_eq!(out[0], footer_no_character());
    assert_eq!(out[1], "Аня");
    assert_eq!(out[2], "Секретарша");
    assert!(
        out[3..]
            .iter()
            .map(String::as_str)
            .eq(footer_additional_character_names()),
        "the built-in service names close the list, in their declared order"
    );
}

/// A name typed in the chapter that the roster already holds must not double the entry: the
/// roster spelling is the one shown, and the chapter copy is dropped case-insensitively.
#[test]
fn a_chapter_name_already_in_the_roster_is_not_repeated() {
    let project = vec!["Аня".to_string()];
    let chapter = vec!["аня".to_string(), "Секретарша".to_string()];
    let out = build_translation_character_names(&project, &chapter);
    // Cyrillic case folding is not ASCII case folding, so compare through `to_lowercase`,
    // the same key the deduplication itself uses.
    let anya_entries = out
        .iter()
        .filter(|name| name.to_lowercase() == "аня")
        .count();
    assert_eq!(anya_entries, 1);
    assert_eq!(out[1], "Аня", "the roster spelling survives");
    assert_eq!(out[2], "Секретарша");
}

/// The bubbles panel indexes `character_names[0]` as its "no character" fallback value and
/// shows the built-ins as combo-box rows, so an empty project must still produce exactly
/// sentinel + the 13 built-in service names.
#[test]
fn an_empty_project_still_yields_the_sentinel_and_every_builtin() {
    let out = build_translation_character_names(&[], &[]);
    assert_eq!(out.len(), 1 + footer_additional_character_names().len());
    assert_eq!(out[0], footer_no_character());
    assert!(
        out[1..]
            .iter()
            .map(String::as_str)
            .eq(footer_additional_character_names())
    );
}

/// The sentinel is a combo-box row, never a free-input candidate: `AutocompleteLine` splices
/// the tail of a candidate from a word boundary, so leaving "(не указан)" in would let a query
/// like "ук" write the fragment `указан)` into the field and, one debounce later, into the
/// bubble as a real character name. The trim must be conditional, not a blind `[1..]`.
#[test]
fn the_footer_gets_the_candidates_without_the_leading_sentinel() {
    let full = build_translation_character_names(&["Аня".to_string()], &[]);
    let offered = footer_autocomplete_candidates(&full);
    assert_eq!(offered.len(), full.len() - 1);
    assert_eq!(offered[0], "Аня");
    assert!(
        !offered.iter().any(|name| name == footer_no_character()),
        "no candidate may carry the sentinel's parentheses into a free-input field"
    );

    // A list that does not start with the sentinel is passed through untouched.
    let no_sentinel = vec!["Аня".to_string(), "Борис".to_string()];
    assert_eq!(footer_autocomplete_candidates(&no_sentinel), &no_sentinel[..]);
    assert!(footer_autocomplete_candidates(&[]).is_empty());
}

/// The revision gate exists so an unrelated bubble edit does not reassemble the list. Nothing
/// else observes that, so assert it directly: a sync whose collected set is unchanged must
/// leave `character_names` byte-identical, and a sync that adds a name must rebuild it.
#[test]
fn an_unchanged_chapter_name_set_does_not_reassemble_the_list() {
    let mut tab = TranslationTabState::default();
    tab.character_names_project = vec!["Аня".to_string()];
    tab.character_names_chapter = vec!["Секретарша".to_string()];
    // A value the assembler could never produce: if it survives, no rebuild happened.
    tab.character_names = vec!["<not reassembled>".to_string()];

    let same = vec![character_bubble(1, Some("text"), "Секретарша")];
    tab.sync_chapter_character_names(&same);
    assert_eq!(
        tab.character_names,
        vec!["<not reassembled>".to_string()],
        "an equal collected set must not touch the assembled list"
    );

    let grown = vec![
        character_bubble(1, Some("text"), "Секретарша"),
        character_bubble(2, Some("text"), "Босс"),
    ];
    tab.sync_chapter_character_names(&grown);
    assert_eq!(
        tab.character_names_chapter,
        vec!["Секретарша".to_string(), "Босс".to_string()]
    );
    assert_eq!(tab.character_names[0], footer_no_character());
    assert_eq!(tab.character_names[1], "Аня");
    assert_eq!(tab.character_names[2], "Секретарша");
    assert_eq!(tab.character_names[3], "Босс");
}

/// A roster reload replaces only the roster half. The chapter names are derived from bubbles,
/// not from `characters.json`, so they must survive it — otherwise every mtime tick of that
/// file would drop the free-form names until the next bubble revision change. This exercises
/// the assignment + `rebuild_character_names` pair that `reload_character_names` performs
/// around its disk read (the read itself needs a real `ProjectData` and is not covered here).
#[test]
fn a_roster_reload_keeps_the_chapter_collected_names() {
    let mut tab = TranslationTabState::default();
    tab.character_names_chapter = vec!["Секретарша".to_string()];
    tab.character_names_project = vec!["Аня".to_string()];
    tab.rebuild_character_names();

    tab.character_names_project = vec!["Борис".to_string()];
    tab.rebuild_character_names();

    assert_eq!(tab.character_names_chapter, vec!["Секретарша".to_string()]);
    assert_eq!(tab.character_names[0], footer_no_character());
    assert_eq!(tab.character_names[1], "Борис", "the roster was replaced");
    assert_eq!(
        tab.character_names[2], "Секретарша",
        "the chapter-collected names survived the reload"
    );
}
