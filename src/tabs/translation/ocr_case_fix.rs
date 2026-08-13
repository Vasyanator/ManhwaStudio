/*
File: src/tabs/translation/ocr_case_fix.rs

Purpose:
Post-OCR "ALL CAPS" normalization. Comic and manhwa lettering fonts are usually
uppercase-only, so an OCR engine returns the whole replica in capitals
("WHEN DID\nWE EVER MEET AT\nTHE ARCADE..."). This module lowers such a result to
ordinary sentence case.

Main responsibilities:
- decide whether a recognized result is entirely uppercase (Latin/Cyrillic only);
- rewrite it so that only the first letter of the text and the first letter after a
  sentence terminator stay uppercase, with ONE exception: the English pronoun `I`
  and its contractions (`I'm`, `I'll`, `I've`, `I'd`) stay capitalized anywhere.

Key functions:
- looks_like_caps_lock(): the detector contract; a SEPARATE public entry point.
- apply_caps_lock_fix(): the unconditional rewrite of an `OcrRecognizeResult`.
- fix_caps_lock_str(): the pure character state machine, state carried by the caller.
- restore_english_i(): the per-word English `I` exception, applied after the machine.

Notes:
GUI-free and I/O-free: both entry points are called from the OCR worker thread
through `ocr::apply_post_ocr_processing`, which DETECTS on the raw engine output and
REWRITES after the user's character substitutions — a substitution repairing an OCR
artifact with a lowercase letter ("0" -> "o") must not be able to disable the fix.
Writing systems are classified with `unicode_script`, never with hand-written
codepoint ranges.
*/

use unicode_script::{Script, UnicodeScript};

use super::ocr::OcrRecognizeResult;

/// Characters that terminate a sentence and therefore make the next letter
/// uppercase. A line break is deliberately NOT one of them: lettering wraps a
/// single sentence across several lines.
const SENTENCE_TERMINATORS: [char; 4] = ['.', '!', '?', '…'];

/// The complete set of words that stay capitalized mid-sentence: the English
/// pronoun `I` and its contractions, in the folded spelling produced by
/// [`fold_word_for_english_i`] (ASCII-lowercase, straight apostrophe).
const ENGLISH_I_WORDS: [&str; 5] = ["i", "i'm", "i'll", "i've", "i'd"];

/// Normalizes a recognized result to sentence case, in place and UNCONDITIONALLY.
///
/// The caller decides whether the fix applies; see [`looks_like_caps_lock`] and
/// `ocr::apply_post_ocr_processing`, which runs the detector on the raw engine
/// output before any character substitution. Calling this on mixed-case text would
/// damage it.
///
/// Each stream is rewritten in two passes: the sentence-case state machine
/// ([`fix_caps_lock_str`]), then the English `I` exception ([`restore_english_i`]).
///
/// `lines` and `text` are rewritten as two independent streams. Every element of
/// `lines` is rewritten in place, so `lines.len()` is structurally preserved and no
/// `\n` is added or removed anywhere.
pub fn apply_caps_lock_fix(result: &mut OcrRecognizeResult) {
    // `lines` is rewritten element by element while THREADING one `expect_capital`
    // state through the whole vector, so a sentence continues across a line break
    // exactly as it does inside one line. Joining on `\n` and splitting back would
    // be wrong: an element may already contain a `\n` (a character substitution
    // runs before this fix and may insert one, and the engine may emit one), and
    // the round trip would silently split it into two lines.
    let mut expect_capital = true;
    for line in &mut result.lines {
        let recased = fix_caps_lock_str(line, &mut expect_capital);
        // Unlike `expect_capital`, the English `I` rule carries NO state across
        // elements: it is decided per WHOLE word, and a word cannot straddle a
        // `lines` boundary — that boundary is a line break, and OCR engines emit
        // one line entry per rendered line without splitting a word across two.
        // So each fragment is folded independently.
        *line = restore_english_i(&recased);
    }

    // `text` is a separate stream with its own fresh state: it may be the
    // newline-joined lines, but it may also be a differently assembled string (RTL
    // reflow, `join_newlines` off), so it is rewritten by its own pass rather than
    // derived from `lines`.
    result.text = fix_caps_lock_text(&result.text);
}

/// Reports whether `result` looks like text typed with Caps Lock held down.
///
/// Judged over the UNION of `text` and every line (an engine may leave one of the
/// two empty), counting only Latin and Cyrillic letters: true when at least one
/// such letter is uppercase and none is lowercase.
///
/// Text in any other writing system — Japanese, Korean, Chinese, Greek, Arabic —
/// contributes no cased Latin/Cyrillic letter at all, so it can never reach the
/// "at least one uppercase" condition and is rejected here without a dedicated
/// branch.
///
/// Must be evaluated on the RAW engine output: applying it after the user's
/// character substitutions would let a repair rule such as `"0" -> "o"` introduce a
/// lowercase letter and veto the fix.
#[must_use]
pub fn looks_like_caps_lock(result: &OcrRecognizeResult) -> bool {
    let mut has_upper = false;
    let sources = std::iter::once(result.text.as_str())
        .chain(result.lines.iter().map(String::as_str));
    for source in sources {
        for ch in source.chars() {
            if !is_target_script_letter(ch) {
                continue;
            }
            // A single lowercase letter proves the engine preserved case.
            if ch.is_lowercase() {
                return false;
            }
            if ch.is_uppercase() {
                has_upper = true;
            }
        }
    }
    has_upper
}

/// Rewrites `text` to sentence case as a self-contained stream, starting a new
/// sentence at its first character and applying the English `I` exception.
///
/// The full two-pass rewrite of one independent fragment: [`fix_caps_lock_str`]
/// with a fresh state, then [`restore_english_i`].
#[must_use]
fn fix_caps_lock_text(text: &str) -> String {
    let mut expect_capital = true;
    let recased = fix_caps_lock_str(text, &mut expect_capital);
    restore_english_i(&recased)
}

/// Rewrites one FRAGMENT of a stream to sentence case, continuing the sentence
/// state the caller passes in and leaving the state ready for the next fragment.
///
/// `expect_capital` is both the entry state (`true` = the next letter starts a
/// sentence) and the exit state; threading it across fragments is what makes a
/// sentence continue over a line break. The state machine:
/// - a Latin/Cyrillic letter consumes the flag (uppercase) or is lowered;
/// - a sentence terminator (`.`, `!`, `?`, `…`) sets the flag;
/// - any other alphanumeric character (a digit, or a letter of another writing
///   system) clears the flag but is copied verbatim;
/// - everything else (spaces, `\n`, quotes, brackets, dashes, apostrophes) is
///   copied verbatim and leaves the flag untouched.
///
/// Case mapping goes through [`char::to_uppercase`]/[`char::to_lowercase`], so
/// expanding mappings are handled correctly. Characters are never added or
/// removed, only re-cased, so a `\n` inside `text` survives in place.
#[must_use]
fn fix_caps_lock_str(text: &str, expect_capital: &mut bool) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if is_target_script_letter(ch) {
            if *expect_capital {
                out.extend(ch.to_uppercase());
                *expect_capital = false;
            } else {
                out.extend(ch.to_lowercase());
            }
        } else if SENTENCE_TERMINATORS.contains(&ch) {
            out.push(ch);
            *expect_capital = true;
        } else if ch.is_alphanumeric() {
            // A digit or a letter of another script is real sentence content: it
            // must consume a pending capital so "END. 3 DAYS" becomes
            // "End. 3 days", not "End. 3 Days".
            out.push(ch);
            *expect_capital = false;
        } else {
            // Separators and punctuation that neither open nor close a sentence:
            // an apostrophe inside DON'T, a line break, a quote, a bracket.
            out.push(ch);
        }
    }
    out
}

/// Re-capitalizes the English pronoun `I` and its contractions everywhere in
/// `text`, undoing the lowercasing the sentence-case pass applied to them.
///
/// A word is the maximal run of alphanumeric characters and apostrophes; both the
/// straight `'` (U+0027) and the typographic `’` (U+2019) count as part of a word,
/// so `I'm` and `I’m` are single words. A word is re-capitalized only when the
/// WHOLE of it folds to one of [`ENGLISH_I_WORDS`] — `is`, `in`, `it` and `i18n`
/// are therefore untouched.
///
/// Changes the case of exactly one ASCII character per matched word and copies
/// everything else verbatim, so the output has the same character count as the
/// input and every `\n` stays where it was.
#[must_use]
fn restore_english_i(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Byte offset where the word currently being scanned started, if inside one.
    let mut word_start: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if word_start.is_none() {
                word_start = Some(idx);
            }
            continue;
        }
        // A non-word character ends the pending word and is itself a separator.
        if let Some(start) = word_start.take() {
            push_word_with_english_i(&mut out, &text[start..idx]);
        }
        out.push(ch);
    }
    // A word running to the very end of the fragment has no closing separator.
    if let Some(start) = word_start {
        push_word_with_english_i(&mut out, &text[start..]);
    }
    out
}

/// Appends `word` to `out`, capitalizing its leading letter when the whole word is
/// the English pronoun `I` or one of its contractions.
fn push_word_with_english_i(out: &mut String, word: &str) {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return;
    };
    // The ASCII-only first-character test is the cheap gate; the folded comparison
    // below is against literal ASCII spellings, which is also WHY no writing-system
    // check is needed here: a Cyrillic `и`/`И` (or any non-Latin letter) can never
    // fold to `i`, so this rule is structurally confined to Latin ASCII.
    if !(first == 'i' || first == 'I') || !is_english_i_word(word) {
        out.push_str(word);
        return;
    }
    // Only the leading letter changes case, and ASCII `i` -> `I` is one character
    // for one character, so the fragment's length and its `\n`s are preserved.
    out.push('I');
    out.push_str(chars.as_str());
}

/// Reports whether the whole `word` is the English pronoun `I` or a contraction of
/// it, comparing case- and apostrophe-insensitively.
#[must_use]
fn is_english_i_word(word: &str) -> bool {
    let folded = fold_word_for_english_i(word);
    ENGLISH_I_WORDS.contains(&folded.as_str())
}

/// Folds `word` for comparison with [`ENGLISH_I_WORDS`]: ASCII characters are
/// lowercased and every apostrophe becomes the straight `'`, so the typographic
/// `I’m` compares equal to `i'm`. Non-ASCII characters are copied unchanged and
/// therefore never match an ASCII spelling.
#[must_use]
fn fold_word_for_english_i(word: &str) -> String {
    word.chars()
        .map(|ch| {
            if is_apostrophe(ch) {
                '\''
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Reports whether `ch` belongs to a word for the English `I` rule: alphanumeric
/// characters and apostrophes, so a contraction is one word.
#[must_use]
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || is_apostrophe(ch)
}

/// Reports whether `ch` is an apostrophe in either spelling OCR produces: the
/// straight `'` (U+0027) or the typographic `’` (U+2019).
#[must_use]
fn is_apostrophe(ch: char) -> bool {
    ch == '\'' || ch == '\u{2019}'
}

/// Reports whether `ch` is a letter of the two writing systems this fix owns
/// (Latin and Cyrillic). Every other script is left untouched by the rewrite.
#[must_use]
fn is_target_script_letter(ch: char) -> bool {
    if !ch.is_alphabetic() {
        return false;
    }
    let script = ch.script();
    script == Script::Latin || script == Script::Cyrillic
}

#[cfg(test)]
mod tests {
    use super::{apply_caps_lock_fix, fix_caps_lock_text, looks_like_caps_lock};
    use crate::tabs::translation::ocr::OcrRecognizeResult;

    fn result_from(lines: &[&str]) -> OcrRecognizeResult {
        OcrRecognizeResult {
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            text: lines.join("\n"),
        }
    }

    /// Mirrors the production gate in `ocr::apply_post_ocr_processing`: detect
    /// first, rewrite only on acceptance. `apply_caps_lock_fix` itself is
    /// unconditional, so the tests that assert "left alone" must gate it here.
    fn apply_if_caps_lock(result: &mut OcrRecognizeResult) {
        if looks_like_caps_lock(result) {
            apply_caps_lock_fix(result);
        }
    }

    // The motivating example: an all-caps multi-line replica becomes one sentence.
    #[test]
    fn lowers_all_caps_multiline_replica() {
        let mut result = result_from(&["WHEN DID", "WE EVER MEET AT", "THE ARCADE..."]);
        apply_caps_lock_fix(&mut result);
        assert_eq!(
            result.lines,
            vec![
                "When did".to_string(),
                "we ever meet at".to_string(),
                "the arcade...".to_string()
            ]
        );
        assert_eq!(result.text, "When did\nwe ever meet at\nthe arcade...");
    }

    // A line break is not a sentence start.
    #[test]
    fn line_break_does_not_capitalize() {
        assert_eq!(fix_caps_lock_text("HELLO\nWORLD"), "Hello\nworld");
    }

    // Each sentence terminator opens a new sentence.
    #[test]
    fn capitalizes_after_sentence_terminators() {
        assert_eq!(fix_caps_lock_text("ONE. TWO! THREE? FOUR"), "One. Two! Three? Four");
        assert_eq!(fix_caps_lock_text("ONE… TWO"), "One… Two");
    }

    // A run of periods keeps the flag set, so the next letter is still capital.
    #[test]
    fn capitalizes_after_multi_dot_ellipsis() {
        assert_eq!(fix_caps_lock_text("WAIT... WHAT"), "Wait... What");
    }

    // Cyrillic follows the same rules as Latin.
    #[test]
    fn handles_cyrillic_sentences() {
        assert_eq!(fix_caps_lock_text("КТО ТЫ? Я НЕ ЗНАЮ."), "Кто ты? Я не знаю.");
    }

    // Mixed-case output means the engine already preserved case: never touch it.
    #[test]
    fn mixed_case_result_is_left_alone() {
        let mut result = result_from(&["Hello WORLD"]);
        assert!(!looks_like_caps_lock(&result));
        apply_if_caps_lock(&mut result);
        assert_eq!(result.lines, vec!["Hello WORLD".to_string()]);
        assert_eq!(result.text, "Hello WORLD");
    }

    // Other writing systems never satisfy the detector.
    #[test]
    fn cjk_result_is_left_alone() {
        let mut result = result_from(&["こんにちは", "안녕하세요"]);
        apply_if_caps_lock(&mut result);
        assert_eq!(
            result.lines,
            vec!["こんにちは".to_string(), "안녕하세요".to_string()]
        );
        assert!(!looks_like_caps_lock(&result));
    }

    // Mixed-script text is NOT byte-identical: the Latin run is re-cased and the
    // characters of the other script are copied through unchanged.
    #[test]
    fn other_scripts_pass_through_a_recased_result() {
        let mut result = result_from(&["HELLO 日本"]);
        apply_if_caps_lock(&mut result);
        assert_eq!(result.lines, vec!["Hello 日本".to_string()]);
        assert_eq!(result.text, "Hello 日本");
    }

    // An apostrophe is not a sentence boundary and not a word boundary either.
    #[test]
    fn apostrophe_inside_word_is_transparent() {
        assert_eq!(fix_caps_lock_text("DON'T GO"), "Don't go");
    }

    // A digit consumes the pending capital.
    #[test]
    fn digit_after_period_consumes_pending_capital() {
        assert_eq!(fix_caps_lock_text("END. 3 DAYS LATER"), "End. 3 days later");
    }

    // An empty result must not panic and must stay empty.
    #[test]
    fn empty_result_is_safe() {
        let mut result = OcrRecognizeResult {
            lines: Vec::new(),
            text: String::new(),
        };
        apply_caps_lock_fix(&mut result);
        assert!(result.lines.is_empty());
        assert!(result.text.is_empty());
    }

    // Line count is an invariant of the rewrite, empty lines included.
    #[test]
    fn line_count_is_preserved() {
        let mut result = result_from(&["ONE", "", "TWO", "THREE"]);
        apply_caps_lock_fix(&mut result);
        assert_eq!(result.lines.len(), 4);
        assert_eq!(
            result.lines,
            vec![
                "One".to_string(),
                String::new(),
                "two".to_string(),
                "three".to_string()
            ]
        );
    }

    // The one exception to sentence case: the standalone English pronoun.
    #[test]
    fn keeps_english_pronoun_i_capitalized() {
        assert_eq!(fix_caps_lock_text("YES, I KNOW"), "Yes, I know");
    }

    // Its contractions are one word each and keep the capital too.
    #[test]
    fn keeps_english_i_contractions_capitalized() {
        assert_eq!(fix_caps_lock_text("I'M NOT ANGRY"), "I'm not angry");
        assert_eq!(fix_caps_lock_text("YES, I'LL GO"), "Yes, I'll go");
        assert_eq!(fix_caps_lock_text("YES, I'VE SEEN IT"), "Yes, I've seen it");
        assert_eq!(fix_caps_lock_text("YES, I'D KNOW"), "Yes, I'd know");
    }

    // OCR often produces the typographic apostrophe U+2019.
    #[test]
    fn typographic_apostrophe_contraction_is_recognized() {
        assert_eq!(fix_caps_lock_text("I’M HERE"), "I’m here");
        assert_eq!(fix_caps_lock_text("YES, I’VE SEEN IT"), "Yes, I’ve seen it");
    }

    // The rule matches WHOLE words only: other words starting with `i` are normal.
    #[test]
    fn other_i_words_are_not_capitalized() {
        assert_eq!(fix_caps_lock_text("IS IT IN THERE"), "Is it in there");
        assert_eq!(fix_caps_lock_text("IT IS INSIDE"), "It is inside");
    }

    // A digit-bearing token is not the pronoun; it must survive untouched.
    #[test]
    fn i18n_token_is_left_as_a_word() {
        assert_eq!(fix_caps_lock_text("I18N"), "I18n");
        assert_eq!(fix_caps_lock_text("USE I18N HERE"), "Use i18n here");
    }

    // Cyrillic `И` is not the English pronoun: only sentence position capitalizes.
    #[test]
    fn cyrillic_i_is_not_the_english_pronoun() {
        assert_eq!(fix_caps_lock_text("И Я ЗНАЮ"), "И я знаю");
        assert_eq!(fix_caps_lock_text("ТЫ И Я"), "Ты и я");
    }

    // The exception applies to every line, not only the first.
    #[test]
    fn english_i_is_restored_on_later_lines() {
        let mut result = result_from(&["I SAW YOU", "AND I LEFT"]);
        apply_caps_lock_fix(&mut result);
        assert_eq!(
            result.lines,
            vec!["I saw you".to_string(), "and I left".to_string()]
        );
        assert_eq!(result.text, "I saw you\nand I left");
    }

    // A `lines` element may itself contain '\n': a character substitution runs
    // before this fix and can insert one ("E" -> "E\n"), and the backend returns
    // whatever the engine produced. Splitting on '\n' would then invent lines.
    #[test]
    fn embedded_newline_does_not_split_a_line() {
        let mut result = OcrRecognizeResult {
            lines: vec!["HE\nLLO".to_string()],
            text: "HE\nLLO".to_string(),
        };
        apply_caps_lock_fix(&mut result);
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines, vec!["He\nllo".to_string()]);
        assert_eq!(result.text, "He\nllo");

        // The English `I` pass must not disturb the embedded break either.
        let mut with_pronoun = OcrRecognizeResult {
            lines: vec!["I\nSAW YOU".to_string()],
            text: "I\nSAW YOU".to_string(),
        };
        apply_caps_lock_fix(&mut with_pronoun);
        assert_eq!(with_pronoun.lines.len(), 1);
        assert_eq!(with_pronoun.lines, vec!["I\nsaw you".to_string()]);
    }

    // The detector reads BOTH fields, so an empty `text` cannot mask the lines.
    #[test]
    fn detector_reads_lines_when_text_is_empty() {
        let result = OcrRecognizeResult {
            lines: vec!["ALL CAPS".to_string()],
            text: String::new(),
        };
        assert!(looks_like_caps_lock(&result));
    }
}
