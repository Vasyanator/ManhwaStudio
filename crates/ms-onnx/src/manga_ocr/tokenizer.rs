/*
File: crates/ms-onnx/src/manga_ocr/tokenizer.rs

Purpose:
BERT WordPiece (char-level) vocabulary for MangaOCR, decode-only. Loads
`vocab.txt` (one token per line, id = 0-based line index) and turns a sequence of
token ids back into text. Encoding is never needed at inference time.

Key structures:
- Vocab: the id -> token table plus decode logic.

Key functions:
- Vocab::load    : read and parse `vocab.txt`.
- Vocab::decode  : id sequence -> concatenated string (specials skipped, `##` stripped).

Notes:
Decode mirrors the Python reference `tokenizer.decode(ids, skip_special_tokens=True)`
followed by `_post_process`'s whitespace collapse: special ids 0..=4
(`[PAD] [UNK] [CLS] [SEP] [MASK]`) are dropped, a leading WordPiece `##` marker is
stripped, and tokens are concatenated with NO separators (all whitespace is
removed downstream anyway, so word-boundary spaces are irrelevant).
*/

use std::path::Path;

use crate::OrtError;

/// Lowest special-token id kept out of decoded text.
const SPECIAL_ID_MIN: usize = 0;
/// Highest special-token id kept out of decoded text (`[MASK]`).
///
/// Ids `0..=4` are `[PAD] [UNK] [CLS] [SEP] [MASK]`; they are skipped on decode,
/// matching `skip_special_tokens=True` in the Python reference.
const SPECIAL_ID_MAX: usize = 4;

/// WordPiece continuation marker stripped from the front of a token on decode.
const WORDPIECE_PREFIX: &str = "##";

/// Decode-only BERT WordPiece vocabulary for MangaOCR.
///
/// `tokens[i]` is the surface string of token id `i`. The table length is the
/// model's vocabulary size (6144 for both the `base` and `2025` MangaOCR exports).
#[derive(Debug, Clone)]
pub struct Vocab {
    /// id -> token surface string, indexed by 0-based line number in `vocab.txt`.
    tokens: Vec<String>,
}

impl Vocab {
    /// Loads a WordPiece vocabulary from a `vocab.txt` file.
    ///
    /// The file must contain one token per line; the token id is the 0-based line
    /// index. Trailing `\r` (CRLF files) is trimmed; a trailing blank final line is
    /// ignored. The vocabulary must be non-empty.
    ///
    /// # Errors
    /// [`OrtError::VocabLoad`] if the file cannot be read or is empty.
    pub fn load(path: &Path) -> Result<Self, OrtError> {
        let text = std::fs::read_to_string(path).map_err(|e| OrtError::VocabLoad {
            path: path.to_path_buf(),
            detail: format!("не удалось прочитать файл: {e}"),
        })?;
        // BERT vocab.txt is one token per line; id = line index. `lines()` drops the
        // line terminator and yields no trailing empty entry for a final newline.
        let tokens: Vec<String> = text.lines().map(str::to_owned).collect();
        if tokens.is_empty() {
            return Err(OrtError::VocabLoad {
                path: path.to_path_buf(),
                detail: "словарь пуст (0 строк)".to_owned(),
            });
        }
        Ok(Self { tokens })
    }

    /// Builds a vocabulary directly from an in-memory token list (test helper).
    #[must_use]
    pub fn from_tokens(tokens: Vec<String>) -> Self {
        Self { tokens }
    }

    /// Number of tokens in the vocabulary (the model's logits width).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the vocabulary is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Decodes a token-id sequence into raw text (pre-post-processing).
    ///
    /// Special ids `0..=4` are skipped, a leading `##` WordPiece marker is stripped,
    /// and surviving token strings are concatenated with no separators. Ids outside
    /// the vocabulary range are skipped defensively (a model whose logits width
    /// equals the vocabulary size cannot emit them, but decode must never panic).
    #[must_use]
    pub fn decode(&self, ids: &[i64]) -> String {
        let mut out = String::new();
        for &id in ids {
            // Negative ids cannot index the table; skip them defensively.
            let Ok(idx) = usize::try_from(id) else {
                continue;
            };
            if (SPECIAL_ID_MIN..=SPECIAL_ID_MAX).contains(&idx) {
                continue;
            }
            let Some(token) = self.tokens.get(idx) else {
                continue;
            };
            let piece = token.strip_prefix(WORDPIECE_PREFIX).unwrap_or(token);
            out.push_str(piece);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic vocabulary: ids 0..=4 are the BERT specials, then real tokens.
    fn synthetic_vocab() -> Vocab {
        Vocab::from_tokens(vec![
            "[PAD]".to_owned(),  // 0
            "[UNK]".to_owned(),  // 1
            "[CLS]".to_owned(),  // 2
            "[SEP]".to_owned(),  // 3
            "[MASK]".to_owned(), // 4
            "日".to_owned(),     // 5
            "本".to_owned(),     // 6
            "##語".to_owned(),   // 7 (WordPiece continuation)
            "!".to_owned(),      // 8
        ])
    }

    #[test]
    fn decode_skips_specials_strips_wordpiece_and_joins_without_spaces() {
        let vocab = synthetic_vocab();
        // [CLS] 日 本 ##語 ! [SEP]  ->  specials dropped, ## stripped, joined.
        let ids = [2i64, 5, 6, 7, 8, 3];
        assert_eq!(vocab.decode(&ids), "日本語!");
    }

    #[test]
    fn decode_skips_all_special_ids() {
        let vocab = synthetic_vocab();
        assert_eq!(vocab.decode(&[0, 1, 2, 3, 4]), "");
    }

    #[test]
    fn decode_ignores_out_of_range_and_negative_ids() {
        let vocab = synthetic_vocab();
        // id 99 is beyond the table, -1 is negative: both skipped, 5 -> "日".
        assert_eq!(vocab.decode(&[99, -1, 5]), "日");
    }

    #[test]
    fn len_and_is_empty() {
        assert_eq!(synthetic_vocab().len(), 9);
        assert!(!synthetic_vocab().is_empty());
        assert!(Vocab::from_tokens(Vec::new()).is_empty());
    }
}
