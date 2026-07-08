/*
File: crates/ms-onnx/src/paddle_ocr/dict.rs

Purpose:
Builds the PaddleOCR recognizer character table used to map CTC class indices to
output characters. Faithful port of `CTCLabelDecoder.__init__` +
`load_character_dict` in `modules/ai_backend/paddle_onnx_runtime.py`.

Key structures:
- CharacterTable : ordered class-index -> character map (index 0 = blank).

Key functions:
- CharacterTable::load       : read a `dict.txt` and build the table.
- CharacterTable::from_lines : build the table from already-read dict lines.

Notes:
Table construction contract (matches Python exactly): start from the dict lines
(one token per line, trailing CR/LF stripped, empty lines skipped); if a bare
space `" "` token is not present, append it at the END; then PREPEND the synthetic
`"blank"` token. Result = `["blank"] + lines + (space?)`, so index 0 is the CTC
blank, indices `1..=lines.len()` are the dict lines in order, and the trailing
space (when appended) is the last index. `num_classes == lines.len() + 2` when the
dict does not already contain a space token.
*/

use std::path::Path;

use crate::OrtError;

/// The synthetic CTC blank token prepended at index 0.
const BLANK_TOKEN: &str = "blank";
/// The space token appended at the end when the dict does not already contain it.
const SPACE_TOKEN: &str = " ";

/// Ordered CTC class-index → character map for the PaddleOCR recognizer.
///
/// Index 0 is always the CTC blank. Construct with [`CharacterTable::load`] (from a
/// `dict.txt`) or [`CharacterTable::from_lines`] (from pre-read lines).
#[derive(Debug, Clone)]
pub struct CharacterTable {
    /// Class tokens in index order; `chars[0]` is the blank sentinel.
    chars: Vec<String>,
}

impl CharacterTable {
    /// Loads and builds the character table from a `dict.txt` file.
    ///
    /// Each line is one token; trailing `\r`/`\n` are stripped and empty lines are
    /// skipped, matching `load_character_dict`.
    ///
    /// # Errors
    /// [`OrtError::PaddleDictLoad`] if the file cannot be read or contains no
    /// non-empty token lines.
    pub fn load(path: &Path) -> Result<Self, OrtError> {
        let content = std::fs::read_to_string(path).map_err(|e| OrtError::PaddleDictLoad {
            path: path.to_path_buf(),
            detail: format!("не удалось прочитать файл: {e}"),
        })?;

        // One token per line; strip trailing CR/LF and skip empty lines (Python
        // rstrips "\r\n" and keeps only truthy tokens).
        let lines: Vec<String> = content
            .lines()
            .map(|line| line.trim_end_matches(['\r', '\n']).to_owned())
            .filter(|line| !line.is_empty())
            .collect();

        if lines.is_empty() {
            return Err(OrtError::PaddleDictLoad {
                path: path.to_path_buf(),
                detail: "словарь не содержит ни одного токена".to_owned(),
            });
        }

        Ok(Self::from_lines(lines))
    }

    /// Builds the table from already-read dict lines (blank prepend, space append).
    ///
    /// The lines must already be stripped/filtered. A bare space token is appended
    /// only if absent; the blank sentinel is always prepended.
    #[must_use]
    pub fn from_lines(mut lines: Vec<String>) -> Self {
        // Append a trailing space class if the dict does not already define one, so
        // recognizers whose training set included a space class stay aligned.
        if !lines.iter().any(|token| token == SPACE_TOKEN) {
            lines.push(SPACE_TOKEN.to_owned());
        }

        let mut chars = Vec::with_capacity(lines.len() + 1);
        chars.push(BLANK_TOKEN.to_owned());
        chars.extend(lines);
        Self { chars }
    }

    /// Number of classes in the table (== the recognizer's `num_classes`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.chars.len()
    }

    /// Whether the table is empty. Always `false` for a table built via the public
    /// constructors (they guarantee at least the blank token), kept for API clarity.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Returns the character for class index `idx`, or `None` if out of range.
    ///
    /// Index 0 returns the blank sentinel; callers must not emit it as output.
    #[must_use]
    pub fn get(&self, idx: usize) -> Option<&str> {
        self.chars.get(idx).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_lines_prepends_blank_and_appends_space() {
        let table = CharacterTable::from_lines(vec!["a".to_owned(), "b".to_owned()]);
        // ["blank", "a", "b", " "]
        assert_eq!(table.len(), 4);
        assert_eq!(table.get(0), Some("blank"));
        assert_eq!(table.get(1), Some("a"));
        assert_eq!(table.get(2), Some("b"));
        assert_eq!(table.get(3), Some(" "));
        assert_eq!(table.get(4), None);
    }

    #[test]
    fn from_lines_keeps_existing_space_without_duplicating() {
        // Dict already contains a space token in the middle: no extra one is added.
        let table = CharacterTable::from_lines(vec!["a".to_owned(), " ".to_owned(), "b".to_owned()]);
        // ["blank", "a", " ", "b"] — num_classes == lines + 1 (blank only).
        assert_eq!(table.len(), 4);
        assert_eq!(table.get(0), Some("blank"));
        assert_eq!(table.get(2), Some(" "));
        assert_eq!(table.get(3), Some("b"));
    }

    #[test]
    fn num_classes_is_lines_plus_two_when_no_space() {
        let lines: Vec<String> = (0..97).map(|i| format!("c{i}")).collect();
        let table = CharacterTable::from_lines(lines);
        // 97 dict lines + blank + appended space = 99.
        assert_eq!(table.len(), 99);
    }
}
