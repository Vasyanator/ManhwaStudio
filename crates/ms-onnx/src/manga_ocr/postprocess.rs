/*
File: crates/ms-onnx/src/manga_ocr/postprocess.rs

Purpose:
Faithful port of the MangaOCR Python `_post_process` text cleanup, including the
`jaconv.h2z(text, ascii=True, digit=True)` half-width -> full-width conversion.

Key functions:
- post_process   : the full ordered cleanup applied to decoded OCR text.
- h2z            : half-width -> full-width (ascii + digit + kana), matching jaconv.

Notes:
Order (must match Python exactly):
  1. remove ALL whitespace (`"".join(text.split())`);
  2. `…` (U+2026) -> `...`;
  3. collapse runs of `[・.]{2,}` to the same count of ASCII `.`;
  4. `jaconv.h2z(text, ascii=True, digit=True)` (kana defaults to True).

The h2z tables (kana seion single-char map and the ordered dakuten two-char
combinations) were extracted verbatim from the installed `jaconv` package so the
conversion is byte-for-byte identical to the reference. The ASCII+digit part
follows jaconv's `H2Z_ALL`: U+0020 -> U+3000, and every U+0021..=U+007E maps to
codepoint + 0xFEE0 (this single rule covers ASCII punctuation, letters, and digits
because HALF_ASCII and HALF_DIGIT together tile the whole 0x21..0x7E range).
*/

/// Half-width katakana voiced/semi-voiced (dakuten/handakuten) combinations, in
/// jaconv's `_conv_dakuten` order: `[half_kana, mark]` -> combined full-width kana.
/// Applied before the single-char map (a matched pair yields a full-width kana that
/// the single-char map then leaves untouched).
const DAKUTEN_PAIRS: &[([u32; 2], u32)] = &[
    ([0xFF76, 0xFF9E], 0x30AC),
    ([0xFF77, 0xFF9E], 0x30AE),
    ([0xFF78, 0xFF9E], 0x30B0),
    ([0xFF79, 0xFF9E], 0x30B2),
    ([0xFF7A, 0xFF9E], 0x30B4),
    ([0xFF7B, 0xFF9E], 0x30B6),
    ([0xFF7C, 0xFF9E], 0x30B8),
    ([0xFF7D, 0xFF9E], 0x30BA),
    ([0xFF7E, 0xFF9E], 0x30BC),
    ([0xFF7F, 0xFF9E], 0x30BE),
    ([0xFF80, 0xFF9E], 0x30C0),
    ([0xFF81, 0xFF9E], 0x30C2),
    ([0xFF82, 0xFF9E], 0x30C5),
    ([0xFF83, 0xFF9E], 0x30C7),
    ([0xFF84, 0xFF9E], 0x30C9),
    ([0xFF8A, 0xFF9E], 0x30D0),
    ([0xFF8B, 0xFF9E], 0x30D3),
    ([0xFF8C, 0xFF9E], 0x30D6),
    ([0xFF8D, 0xFF9E], 0x30D9),
    ([0xFF8E, 0xFF9E], 0x30DC),
    ([0xFF8A, 0xFF9F], 0x30D1),
    ([0xFF8B, 0xFF9F], 0x30D4),
    ([0xFF8C, 0xFF9F], 0x30D7),
    ([0xFF8D, 0xFF9F], 0x30DA),
    ([0xFF8E, 0xFF9F], 0x30DD),
    ([0xFF73, 0xFF9E], 0x30F4),
];

/// Half-width kana "seion" (unvoiced) single-char map: half codepoint -> full
/// codepoint. Extracted verbatim from jaconv's `H2Z_ALL` (kana portion). A few
/// entries map a codepoint to itself (chars jaconv lists in both half and full
/// tables); those are harmless no-ops kept for exactness.
const KANA_SEION_MAP: &[(u32, u32)] = &[
    (0xFF67, 0x30A1),
    (0xFF71, 0x30A2),
    (0xFF68, 0x30A3),
    (0xFF72, 0x30A4),
    (0xFF69, 0x30A5),
    (0xFF73, 0x30A6),
    (0xFF6A, 0x30A7),
    (0xFF74, 0x30A8),
    (0xFF6B, 0x30A9),
    (0xFF75, 0x30AA),
    (0xFF76, 0x30AB),
    (0xFF77, 0x30AD),
    (0xFF78, 0x30AF),
    (0xFF79, 0x30B1),
    (0xFF7A, 0x30B3),
    (0xFF7B, 0x30B5),
    (0xFF7C, 0x30B7),
    (0xFF7D, 0x30B9),
    (0xFF7E, 0x30BB),
    (0xFF7F, 0x30BD),
    (0xFF80, 0x30BF),
    (0xFF81, 0x30C1),
    (0xFF6F, 0x30C3),
    (0xFF82, 0x30C4),
    (0xFF83, 0x30C6),
    (0xFF84, 0x30C8),
    (0xFF85, 0x30CA),
    (0xFF86, 0x30CB),
    (0xFF87, 0x30CC),
    (0xFF88, 0x30CD),
    (0xFF89, 0x30CE),
    (0xFF8A, 0x30CF),
    (0xFF8B, 0x30D2),
    (0xFF8C, 0x30D5),
    (0xFF8D, 0x30D8),
    (0xFF8E, 0x30DB),
    (0xFF8F, 0x30DE),
    (0xFF90, 0x30DF),
    (0xFF91, 0x30E0),
    (0xFF92, 0x30E1),
    (0xFF93, 0x30E2),
    (0xFF6C, 0x30E3),
    (0xFF94, 0x30E4),
    (0xFF6D, 0x30E5),
    (0xFF95, 0x30E6),
    (0xFF6E, 0x30E7),
    (0xFF96, 0x30E8),
    (0xFF97, 0x30E9),
    (0xFF98, 0x30EA),
    (0xFF99, 0x30EB),
    (0xFF9A, 0x30EC),
    (0xFF9B, 0x30ED),
    (0xFF9C, 0x30EF),
    (0xFF66, 0x30F2),
    (0xFF9D, 0x30F3),
    (0xFF70, 0x30FC),
    (0x30EE, 0x30EE),
    (0x30F0, 0x30F0),
    (0x30F1, 0x30F1),
    (0x30F5, 0x30F5),
    (0x30F6, 0x30F6),
    (0x30FD, 0x30FD),
    (0x30FE, 0x30FE),
    (0xFF65, 0x30FB),
    (0xFF62, 0x300C),
    (0xFF63, 0x300D),
    (0xFF61, 0x3002),
    (0xFF64, 0x3001),
];

/// jaconv full-width-ideographic space (half-width U+0020 maps here, NOT +0xFEE0).
const FULLWIDTH_SPACE: u32 = 0x3000;
/// Offset between the half-width ASCII block (U+0021..U+007E) and its full-width
/// form (U+FF01..U+FF5E).
const HALFWIDTH_TO_FULLWIDTH_OFFSET: u32 = 0xFEE0;
/// Middle dot (`・`, U+30FB) — part of the dot-run collapse character class.
const MIDDLE_DOT: char = '\u{30FB}';
/// Horizontal ellipsis (`…`, U+2026), expanded to three ASCII dots.
const ELLIPSIS: char = '\u{2026}';

/// Applies the full MangaOCR post-processing pipeline to decoded text.
///
/// Ordered exactly as the Python reference `_post_process`: strip all whitespace,
/// expand `…` to `...`, collapse runs of `・`/`.` (length >= 2) to that many ASCII
/// dots, then apply [`h2z`] (half-width -> full-width for ASCII, digits, and kana).
#[must_use]
pub fn post_process(text: &str) -> String {
    // 1. Remove all whitespace: Python `"".join(text.split())`.
    let without_spaces: String = text.split_whitespace().collect();
    // 2. `…` -> `...`.
    let without_ellipsis = without_spaces.replace(ELLIPSIS, "...");
    // 3. Collapse runs of `[・.]{2,}` to the same count of ASCII '.'.
    let normalized_dots = collapse_dot_runs(&without_ellipsis);
    // 4. jaconv h2z(ascii=True, digit=True) (kana defaults to True).
    h2z(&normalized_dots)
}

/// Collapses each maximal run of `・`/`.` of length >= 2 to that many ASCII `.`.
///
/// A lone `・` or `.` is left unchanged, matching the regex `[・.]{2,}` which only
/// matches runs of two or more. Mixed runs (e.g. `.・.`) count as one run.
fn collapse_dot_runs(text: &str) -> String {
    let is_dot = |c: char| c == '.' || c == MIDDLE_DOT;
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if is_dot(chars[i]) {
            // Measure the maximal run of dot-class characters starting at `i`.
            let mut j = i;
            while j < chars.len() && is_dot(chars[j]) {
                j += 1;
            }
            let run_len = j - i;
            if run_len >= 2 {
                for _ in 0..run_len {
                    out.push('.');
                }
            } else {
                out.push(chars[i]);
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// jaconv `h2z(text, ascii=True, digit=True)` (kana defaults to True).
///
/// Converts half-width forms to full-width: ASCII punctuation/letters/digits
/// (U+0021..=U+007E -> +0xFEE0, U+0020 -> U+3000), half-width katakana (single-char
/// seion map), and half-width dakuten/handakuten two-char combinations. Other
/// characters pass through unchanged.
#[must_use]
pub fn h2z(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        // Try a two-char dakuten/handakuten combination first (jaconv applies
        // `_conv_dakuten` before the single-char map).
        if i + 1 < chars.len() {
            let pair = [chars[i] as u32, chars[i + 1] as u32];
            if let Some(full) = lookup_dakuten(pair) {
                out.push(full);
                i += 2;
                continue;
            }
        }
        out.push(single_char_h2z(chars[i]));
        i += 1;
    }
    out
}

/// Looks up a half-width dakuten/handakuten pair, returning the combined kana.
fn lookup_dakuten(pair: [u32; 2]) -> Option<char> {
    DAKUTEN_PAIRS
        .iter()
        .find(|(key, _)| *key == pair)
        .and_then(|&(_, full)| char::from_u32(full))
}

/// Converts a single half-width character to its full-width form (ASCII, digit, or
/// kana seion); returns the input unchanged if it has no mapping.
fn single_char_h2z(c: char) -> char {
    let u = c as u32;
    if u == 0x20 {
        return char::from_u32(FULLWIDTH_SPACE).unwrap_or(c);
    }
    if (0x21..=0x7E).contains(&u) {
        // The half-width ASCII block maps 1:1 to full-width by a fixed offset.
        return char::from_u32(u + HALFWIDTH_TO_FULLWIDTH_OFFSET).unwrap_or(c);
    }
    if let Some(&(_, full)) = KANA_SEION_MAP.iter().find(|(half, _)| *half == u) {
        return char::from_u32(full).unwrap_or(c);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected outputs below were produced by running the installed reference
    // `jaconv.h2z(..., ascii=True, digit=True)` / `_post_process`; they are ground
    // truth, not hand-guessed.

    #[test]
    fn h2z_ascii_and_digits() {
        assert_eq!(h2z("Abc123"), "\u{FF21}\u{FF42}\u{FF43}\u{FF11}\u{FF12}\u{FF13}");
        assert_eq!(h2z("Abc123"), "Ａｂｃ１２３");
    }

    #[test]
    fn h2z_ascii_punctuation() {
        assert_eq!(h2z("!?~"), "！？～");
    }

    #[test]
    fn h2z_halfwidth_kana_dakuten_and_handakuten() {
        assert_eq!(h2z("ｶﾞｷﾞ"), "ガギ");
        assert_eq!(h2z("ﾊﾟﾋﾟ"), "パピ");
        assert_eq!(h2z("ｳﾞ"), "ヴ");
        assert_eq!(h2z("ｱｲｳ"), "アイウ");
    }

    #[test]
    fn h2z_leaves_full_width_and_hiragana_untouched() {
        assert_eq!(h2z("こんにちは"), "こんにちは");
        assert_eq!(h2z("あ・い"), "あ・い");
    }

    #[test]
    fn post_process_ellipsis_and_dot_runs() {
        assert_eq!(post_process("…"), "．．．");
        assert_eq!(post_process("・・・"), "．．．");
        assert_eq!(post_process("..."), "．．．");
    }

    #[test]
    fn post_process_single_middle_dot_is_kept() {
        assert_eq!(post_process("あ・い"), "あ・い");
    }

    #[test]
    fn post_process_removes_all_whitespace_and_full_pipeline() {
        // Verified against Python: "A B\tC\n1・2…3" -> "ＡＢＣ１・２．．．３".
        assert_eq!(post_process("A B\tC\n1・2…3"), "ＡＢＣ１・２．．．３");
    }

    #[test]
    fn post_process_mixed_japanese_unchanged() {
        assert_eq!(post_process("こんにちは"), "こんにちは");
    }
}
