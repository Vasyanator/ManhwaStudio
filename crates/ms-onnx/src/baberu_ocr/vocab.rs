/*
File: crates/ms-onnx/src/baberu_ocr/vocab.rs

Purpose:
The Baberu OCR character vocabulary (`tokenizer/vocab.json`): id -> char decoding and the
"content" classification the decoder's run caps depend on.

Key structures:
- BaberuVocab : chars indexed by `id - 4` plus a parallel content flag.

Key functions:
- BaberuVocab::from_json  : parse the JSON list of single-character strings.
- BaberuVocab::is_content : whether an id is a letter/number ("content") token.
- BaberuVocab::decode     : ids -> text (ids 0..3 are special and produce nothing).

Notes:
Port of the reference `Vocab` class in upstream `onnx_infer.py`: ids 0..3 are
`<pad>/<bos>/<eos>/<unk>`; id >= 4 maps to `charset[id - 4]`. A char is "content" when
its Unicode general category is L* or N* and it is none of `ーｰ〜~` (Python's
`unicodedata.category(ch)[0] in "LN"`). The category comes from
`unicode-general-category` (Unicode 16) while the reference ran on Python's Unicode
14; the real vocab holds no code point whose category differs between the two (pinned
by the opt-in `noncontent_ids.json` check). `char::is_alphanumeric` is a different set
and must not be used.
*/

use std::path::Path;

use unicode_general_category::get_general_category;

use crate::OrtError;

/// Number of special ids before the first character (`<pad>`, `<bos>`, `<eos>`, `<unk>`).
pub(crate) const SPECIAL_IDS: u32 = 4;

/// Letters/numbers the reference nevertheless treats as symbols for the run cap
/// (prolonged-sound marks and the wave dash).
const NON_CONTENT_CHARS: [char; 4] = ['ー', 'ｰ', '〜', '~'];

/// The Baberu character vocabulary and its content classification.
#[derive(Debug)]
pub(crate) struct BaberuVocab {
    /// Characters indexed by `id - SPECIAL_IDS`.
    chars: Vec<char>,
    /// `content[i]` is the content flag of `chars[i]`.
    content: Vec<bool>,
}

impl BaberuVocab {
    /// Parses `vocab.json` bytes: a JSON array whose every entry is a one-character
    /// string. `source` is the file path, used only in error messages.
    ///
    /// # Errors
    /// [`OrtError::BaberuVocabLoad`] for malformed JSON, a non-array document, an entry
    /// that is not a string of exactly one character, or an empty list.
    pub(crate) fn from_json(bytes: &[u8], source: &Path) -> Result<Self, OrtError> {
        let fail = |detail: String| OrtError::BaberuVocabLoad {
            path: source.to_path_buf(),
            detail,
        };
        let entries: Vec<String> =
            serde_json::from_slice(bytes).map_err(|e| fail(format!("некорректный JSON: {e}")))?;
        if entries.is_empty() {
            return Err(fail("словарь пуст".to_owned()));
        }
        let mut chars = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let mut it = entry.chars();
            match (it.next(), it.next()) {
                (Some(ch), None) => chars.push(ch),
                _ => {
                    return Err(fail(format!(
                        "запись #{index} ({entry:?}) должна быть ровно одним символом"
                    )));
                }
            }
        }
        // The id space must fit u32 together with the special ids.
        u32::try_from(chars.len())
            .ok()
            .and_then(|len| len.checked_add(SPECIAL_IDS))
            .ok_or_else(|| fail(format!("слишком большой словарь: {} записей", chars.len())))?;
        let content = chars.iter().map(|&ch| is_content_char(ch)).collect();
        Ok(Self { chars, content })
    }

    /// Total number of ids the model's logits row must have (chars + 4 specials).
    #[must_use]
    pub(crate) fn vocab_size(&self) -> usize {
        // `4` is `SPECIAL_IDS`; cannot overflow: `from_json` proved the sum fits in u32.
        self.chars.len().saturating_add(4)
    }

    /// Whether `id` is a content token (a letter or number). Special ids and ids past
    /// the vocabulary are never content.
    #[must_use]
    pub(crate) fn is_content(&self, id: u32) -> bool {
        char_index(id).and_then(|i| self.content.get(i)).copied().unwrap_or(false)
    }

    /// Decodes `ids` to text: ids 0..3 produce nothing, ids >= 4 map to `chars[id - 4]`.
    ///
    /// An id past the vocabulary is skipped, like the reference's `id2ch.get(i, "")`;
    /// the engine never produces one because it checks the logits width against
    /// [`BaberuVocab::vocab_size`].
    #[must_use]
    pub(crate) fn decode(&self, ids: &[u32]) -> String {
        ids.iter()
            .filter_map(|&id| char_index(id).and_then(|i| self.chars.get(i)))
            .collect()
    }
}

/// `id - 4` as an index into the char table, or `None` for a special id.
fn char_index(id: u32) -> Option<usize> {
    id.checked_sub(SPECIAL_IDS).and_then(|i| usize::try_from(i).ok())
}

/// The reference content rule: general category L* or N*, minus [`NON_CONTENT_CHARS`].
fn is_content_char(ch: char) -> bool {
    if NON_CONTENT_CHARS.contains(&ch) {
        return false;
    }
    // The two-letter abbreviation ("Lo", "Nd", ...) starts with the major class letter,
    // which is exactly what `unicodedata.category(ch)[0]` reads.
    matches!(get_general_category(ch).abbreviation().as_bytes().first(), Some(b'L' | b'N'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab(json: &str) -> Result<BaberuVocab, OrtError> {
        BaberuVocab::from_json(json.as_bytes(), Path::new("vocab.json"))
    }

    #[test]
    fn decodes_ids_and_skips_specials() {
        let Ok(v) = vocab(r#"["a", "\n", "ド", "！"]"#) else {
            panic!("valid vocab");
        };
        assert_eq!(v.vocab_size(), 8);
        // 0..3 are specials; 4 -> 'a', 5 -> '\n', 6 -> 'ド'; 99 is past the vocab.
        assert_eq!(v.decode(&[1, 4, 5, 3, 6, 0, 99, 2]), "a\nド");
    }

    #[test]
    fn content_classification_matches_the_reference_rule() {
        let Ok(v) = vocab(r#"["a", "7", "ド", "漢", "ー", "~", "〜", "ｰ", "！", " ", "Ⅻ", "ﾞ"]"#) else {
            panic!("valid vocab");
        };
        let content: Vec<bool> = (4..16).map(|id| v.is_content(id)).collect();
        // a(Ll) 7(Nd) ド(Lo) 漢(Lo) are content; ー/~/〜/ｰ are excluded explicitly;
        // ！(Po) and space(Zs) are symbols; Ⅻ(Nl) and ﾞ(Lm) are content.
        assert_eq!(
            content,
            [true, true, true, true, false, false, false, false, false, false, true, true]
        );
        for special in 0..4 {
            assert!(!v.is_content(special));
        }
        assert!(!v.is_content(1000));
    }

    /// Opt-in check against the REAL vocabulary: with `MS_BABERU_MODEL_DIR` set to a
    /// Baberu model directory, the Rust content set must equal the reference's (the
    /// non-content ids recorded by `tools/make_baberu_fixtures.py` with Python's
    /// `unicodedata`). Skips with one line when the variable is unset.
    #[test]
    fn real_vocab_content_set_matches_the_python_reference() {
        let Some(dir) = std::env::var_os("MS_BABERU_MODEL_DIR") else {
            eprintln!("skipping: MS_BABERU_MODEL_DIR is not set");
            return;
        };
        let path = Path::new(&dir).join("tokenizer").join("vocab.json");
        let bytes = std::fs::read(&path);
        let Ok(bytes) = bytes else {
            panic!("cannot read {}: {bytes:?}", path.display());
        };
        let parsed = BaberuVocab::from_json(&bytes, &path);
        let Ok(v) = parsed else {
            panic!("real vocab must parse: {parsed:?}");
        };
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/baberu/noncontent_ids.json"))
                .unwrap_or(serde_json::Value::Null);
        let expected: Vec<u64> = fixture["noncontent_ids"]
            .as_array()
            .map(|ids| ids.iter().filter_map(serde_json::Value::as_u64).collect())
            .unwrap_or_default();
        assert_eq!(fixture["vocab_len"].as_u64(), u64::try_from(v.chars.len()).ok());
        let size = u32::try_from(v.vocab_size()).unwrap_or(0);
        let actual: Vec<u64> = (SPECIAL_IDS..size)
            .filter(|&id| !v.is_content(id))
            .map(u64::from)
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_malformed_vocabularies() {
        assert!(matches!(vocab("{"), Err(OrtError::BaberuVocabLoad { .. })));
        assert!(matches!(vocab(r#"{"a": 1}"#), Err(OrtError::BaberuVocabLoad { .. })));
        assert!(matches!(vocab("[]"), Err(OrtError::BaberuVocabLoad { .. })));
        assert!(matches!(vocab(r#"["a", "bc"]"#), Err(OrtError::BaberuVocabLoad { .. })));
        assert!(matches!(vocab(r#"["a", ""]"#), Err(OrtError::BaberuVocabLoad { .. })));
        assert!(matches!(vocab(r#"["a", 5]"#), Err(OrtError::BaberuVocabLoad { .. })));
    }
}
