/*
File: crates/ms-tab-translation/src/mt_limits/split.rs

Purpose:
Splits a text that is too long for one machine-translation request into chunks that each fit
`LaneLimits::max_request_chars`, remembering the exact separator that followed every
chunk, so the translated chunks can be joined back with the original line breaks and spacing.

Key items:
- `RequestChunk`: one request's text plus the separator that followed it in the source.
- `SizeUnit`: how a request's size is measured (`char`s for the built-in providers, UTF-16
  code units for the browser extension, whose check is JavaScript's `String.length`).
- `split_for_request()` (chars) / `split_for_request_in()` (any unit): the cut rule (below).
- `join_chunks()`: concatenates translated chunks with their separators.

Cut rule (lengths are measured in the caller's `SizeUnit`, never bytes):
1. Whole lines are grouped greedily; the separator of a line is its exact newline run.
2. A line still over the limit is cut after sentence ends (`.!?。！？…` runs): the separator is
   the whitespace run that follows; after a full-width `。！？` with no whitespace (CJK) it is
   empty. A terminator directly followed by a non-space (`3.14`, `a.b`) is not a sentence end.
3. A piece still over the limit is cut at its last whitespace inside the limit, else at a char
   boundary (empty separator). A piece always keeps at least one char, so a single char wider
   than the limit (an astral char under `Utf16` with a limit of 1) is a chunk of its own.
Pieces are then regrouped greedily, so neighbouring short sentences share one request.
Invariant: concatenating every `text + separator` reproduces the input exactly. A chunk can be
empty or whitespace-only (blank lines, leading spaces); callers pass such chunks through
untranslated.
*/

/// How the size of a request text is measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeUnit {
    /// Unicode scalar values (`char`s): the built-in providers' limits.
    Chars,
    /// UTF-16 code units: the browser extension measures a request with JavaScript's
    /// `String.length`, where a char outside the Basic Multilingual Plane counts twice.
    Utf16,
}

impl SizeUnit {
    /// The size of `c` in this unit.
    const fn of_char(self, c: char) -> usize {
        match self {
            Self::Chars => 1,
            Self::Utf16 => c.len_utf16(),
        }
    }

    /// The size of `text` in this unit.
    #[must_use]
    pub(crate) fn measure(self, text: &str) -> usize {
        match self {
            Self::Chars => text.chars().count(),
            Self::Utf16 => text.encode_utf16().count(),
        }
    }
}

/// One request's worth of text and the separator that followed it in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestChunk {
    /// The text to translate (at most the limit, in the caller's `SizeUnit`).
    pub(crate) text: String,
    /// The source text between this chunk and the next (newlines, spaces), kept verbatim and
    /// never sent; empty after the last chunk.
    pub(crate) separator: String,
}

/// A cut inside the source: the piece's text ends at byte `text_end`, its separator at
/// `sep_end` (where the next piece starts).
#[derive(Debug, Clone, Copy)]
struct Piece {
    text_end: usize,
    sep_end: usize,
}

/// Sentence terminators (rule 2).
const SENTENCE_END: &[char] = &['.', '!', '?', '。', '！', '？', '…'];
/// Full-width terminators that end a sentence even without following whitespace.
const FULL_WIDTH_END: &[char] = &['。', '！', '？'];

/// Splits `text` into chunks of at most `max_chars` chars (`max_chars == 0` is treated as 1)
/// by the rule in the file header. A text that already fits is returned as one chunk.
#[must_use]
pub(crate) fn split_for_request(text: &str, max_chars: usize) -> Vec<RequestChunk> {
    split_for_request_in(text, max_chars, SizeUnit::Chars)
}

/// [`split_for_request`] with the limit `max_size` measured in `unit` (`max_size == 0` is
/// treated as 1). Every chunk fits `max_size`, except a single char wider than the limit
/// (rule 3).
#[must_use]
pub(crate) fn split_for_request_in(text: &str, max_size: usize, unit: SizeUnit) -> Vec<RequestChunk> {
    let max_size = max_size.max(1);
    if unit.measure(text) <= max_size {
        return vec![RequestChunk { text: text.to_owned(), separator: String::new() }];
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let line_end = run_end(text, start, |c| !is_newline(c));
        let sep_end = run_end(text, line_end, is_newline);
        if size_of(text, start, line_end, unit) <= max_size {
            pieces.push(Piece { text_end: line_end, sep_end });
        } else {
            let mut piece_start = start;
            for sentence in sentence_pieces(text, start, line_end, sep_end) {
                hard_cut(text, piece_start, sentence, max_size, unit, &mut pieces);
                piece_start = sentence.sep_end;
            }
        }
        start = sep_end;
    }
    group(text, &pieces, max_size, unit)
}

/// Joins translated chunks, each followed by its original separator. `parts` yields
/// `(translated text, separator)` in source order.
#[must_use]
pub(crate) fn join_chunks<'c>(parts: impl IntoIterator<Item = (&'c str, &'c str)>) -> String {
    parts.into_iter().fold(String::new(), |mut joined, (translated, separator)| {
        joined.push_str(translated);
        joined.push_str(separator);
        joined
    })
}

/// Whether `c` belongs to a line separator run.
fn is_newline(c: char) -> bool {
    matches!(c, '\n' | '\r')
}

/// Byte offset where the run of chars matching `keep` that starts at `from` ends.
fn run_end(text: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    text[from..].char_indices().find(|&(_, c)| !keep(c)).map_or(text.len(), |(offset, _)| from + offset)
}

/// Size of `text[start..end]` in `unit`.
fn size_of(text: &str, start: usize, end: usize, unit: SizeUnit) -> usize {
    unit.measure(&text[start..end])
}

/// Byte offset of the first char of `text[start..end]` that no longer fits `max_size` (the
/// window keeps at least one char); `None` when the whole range fits.
fn window_end(text: &str, start: usize, end: usize, max_size: usize, unit: SizeUnit) -> Option<usize> {
    let mut used = 0usize;
    for (offset, c) in text[start..end].char_indices() {
        let size = unit.of_char(c);
        if offset > 0 && used.saturating_add(size) > max_size {
            return Some(start + offset);
        }
        used = used.saturating_add(size);
    }
    None
}

/// Rule 2: the sentence pieces of the line `text[start..line_end]`; the last one carries the
/// line's separator up to `sep_end`.
fn sentence_pieces(text: &str, start: usize, line_end: usize, sep_end: usize) -> Vec<Piece> {
    let line = &text[start..line_end];
    let mut pieces = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        if !SENTENCE_END.contains(&c) {
            continue;
        }
        // Consume the whole terminator run ("?!", "...").
        let mut last = c;
        let mut run_end_offset = offset + c.len_utf8();
        while let Some(&(next_offset, next)) = chars.peek()
            && SENTENCE_END.contains(&next)
        {
            last = next;
            run_end_offset = next_offset + next.len_utf8();
            chars.next();
        }
        let text_end = start + run_end_offset;
        let ws_end = run_end(&text[..line_end], text_end, char::is_whitespace);
        if ws_end == line_end {
            // The sentence (and its trailing spaces) ends the line: the line's own last piece.
            break;
        }
        if ws_end > text_end || FULL_WIDTH_END.contains(&last) {
            pieces.push(Piece { text_end, sep_end: ws_end });
            while chars.peek().is_some_and(|&(next_offset, _)| start + next_offset < ws_end) {
                chars.next();
            }
        }
    }
    let last_text_end = if let Some(previous) = pieces.last() {
        trailing_text_end(text, previous.sep_end, line_end)
    } else {
        trailing_text_end(text, start, line_end)
    };
    pieces.push(Piece { text_end: last_text_end, sep_end });
    pieces
}

/// End of the text of the line's last piece `text[from..line_end]`: before its trailing
/// whitespace, which then belongs to the separator (a piece of only whitespace keeps it as text).
fn trailing_text_end(text: &str, from: usize, line_end: usize) -> usize {
    let trimmed = text[from..line_end].trim_end().len();
    if trimmed == 0 { line_end } else { from + trimmed }
}

/// Rule 3: pushes `piece` (text from `start`), cut at whitespace or char boundaries until
/// every part fits `max_size` (in `unit`).
fn hard_cut(text: &str, mut start: usize, piece: Piece, max_size: usize, unit: SizeUnit, out: &mut Vec<Piece>) {
    loop {
        let Some(window_end) = window_end(text, start, piece.text_end, max_size, unit) else {
            out.push(Piece { text_end: piece.text_end, sep_end: piece.sep_end });
            return;
        };
        let space = text[start..window_end].char_indices().rev().find(|&(offset, c)| offset > 0 && c.is_whitespace()).map(|(offset, _)| start + offset);
        match space {
            Some(space) => {
                // The space starts a run; the run (possibly reaching past the window) is the
                // separator.
                let space_end = run_end(&text[..piece.text_end], space, char::is_whitespace);
                if space_end >= piece.text_end {
                    out.push(Piece { text_end: space, sep_end: piece.sep_end });
                    return;
                }
                out.push(Piece { text_end: space, sep_end: space_end });
                start = space_end;
            }
            None => {
                out.push(Piece { text_end: window_end, sep_end: window_end });
                start = window_end;
            }
        }
    }
}

/// Regroups consecutive pieces greedily into chunks of at most `max_size` (in `unit`; a
/// chunk's inner separators count towards it).
fn group(text: &str, pieces: &[Piece], max_size: usize, unit: SizeUnit) -> Vec<RequestChunk> {
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut current: Option<(Piece, usize)> = None;
    let mut piece_start = 0;
    for &piece in pieces {
        let piece_len = size_of(text, piece_start, piece.text_end, unit);
        current = match current {
            Some((open, open_len)) => {
                let joined_len = open_len + size_of(text, open.text_end, piece_start, unit) + piece_len;
                if joined_len <= max_size {
                    Some((piece, joined_len))
                } else {
                    chunks.push(chunk(text, chunk_start, open));
                    chunk_start = piece_start;
                    Some((piece, piece_len))
                }
            }
            None => Some((piece, piece_len)),
        };
        piece_start = piece.sep_end;
    }
    if let Some((open, _)) = current {
        chunks.push(chunk(text, chunk_start, open));
    }
    chunks
}

/// The chunk `text[start..last.text_end]` with separator up to `last.sep_end`.
fn chunk(text: &str, start: usize, last: Piece) -> RequestChunk {
    RequestChunk { text: text[start..last.text_end].to_owned(), separator: text[last.text_end..last.sep_end].to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str, max_chars: usize) -> Vec<RequestChunk> {
        let chunks = split_for_request(text, max_chars);
        assert_eq!(join_chunks(chunks.iter().map(|chunk| (chunk.text.as_str(), chunk.separator.as_str()))), text, "round trip");
        for chunk in &chunks {
            assert!(chunk.text.chars().count() <= max_chars.max(1), "chunk over the limit: {:?}", chunk.text);
        }
        chunks
    }

    fn texts(chunks: &[RequestChunk]) -> Vec<&str> {
        chunks.iter().map(|chunk| chunk.text.as_str()).collect()
    }

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(round_trip("Hello.\nWorld", 50), [RequestChunk { text: "Hello.\nWorld".into(), separator: String::new() }]);
        assert_eq!(round_trip("", 5).len(), 1);
    }

    #[test]
    fn lines_are_grouped_and_newline_runs_preserved() {
        let text = "aaaa\nbbbb\r\n\r\ncccc\n\n\ndddd";
        let chunks = round_trip(text, 10);
        assert_eq!(texts(&chunks), ["aaaa\nbbbb", "cccc", "dddd"]);
        assert_eq!(chunks[0].separator, "\r\n\r\n");
        assert_eq!(chunks[1].separator, "\n\n\n");
        assert_eq!(chunks[2].separator, "");
    }

    #[test]
    fn twelve_thousand_chars_with_newlines_make_three_requests() {
        let line = "x".repeat(3999);
        let text = [line.as_str(); 3].join("\n");
        let text = format!("{text}\n{}", "y".repeat(10));
        let chunks = round_trip(&text, 5000);
        assert_eq!(chunks.len(), 3);
        // The newlines between groups are separators, never sent.
        assert!(chunks.iter().all(|chunk| !chunk.text.starts_with('\n')));
    }

    #[test]
    fn long_line_is_cut_after_sentence_ends() {
        let text = "One two. Three four?! Five six… Seven eight";
        let chunks = round_trip(text, 12);
        assert_eq!(texts(&chunks), ["One two.", "Three four?!", "Five six…", "Seven eight"]);
        assert!(chunks[..3].iter().all(|chunk| chunk.separator == " "));
        // Neighbouring sentences share a request when they fit.
        assert_eq!(texts(&round_trip(text, 22)), ["One two. Three four?!", "Five six… Seven eight"]);
    }

    #[test]
    fn decimal_points_are_not_sentence_ends() {
        let chunks = round_trip("Pi is 3.14159 and e is 2.71828 ok", 20);
        assert_eq!(texts(&chunks), ["Pi is 3.14159 and e", "is 2.71828 ok"]);
    }

    #[test]
    fn cjk_is_cut_after_full_width_terminators_without_separator() {
        let text = "今日は晴れです。明日は雨です！明後日は？";
        let chunks = round_trip(text, 8);
        assert_eq!(texts(&chunks), ["今日は晴れです。", "明日は雨です！", "明後日は？"]);
        assert!(chunks.iter().all(|chunk| chunk.separator.is_empty()));
    }

    #[test]
    fn long_sentence_is_cut_at_whitespace() {
        let text = "alpha beta gamma delta epsilon";
        let chunks = round_trip(text, 12);
        assert_eq!(texts(&chunks), ["alpha beta", "gamma delta", "epsilon"]);
    }

    #[test]
    fn text_without_whitespace_is_cut_at_char_boundaries() {
        let text = "абвгдежзийклмнопрст";
        let chunks = round_trip(text, 5);
        assert_eq!(texts(&chunks), ["абвгд", "ежзий", "клмно", "прст"]);
        assert!(chunks.iter().all(|chunk| chunk.separator.is_empty()));
        assert_eq!(round_trip("abc", 0).len(), 3);
    }

    #[test]
    fn trailing_spaces_and_blank_lines_survive() {
        let text = "  First sentence here.   Second one follows.  \n\n   \nThird line is long enough to cut.";
        let chunks = round_trip(text, 20);
        assert!(chunks.len() > 3);
        let mixed = "word ".repeat(30);
        round_trip(&mixed, 7);
        round_trip("a.\n.b. . .\n\n…", 2);
    }

    #[test]
    fn generated_texts_round_trip_within_the_limit() {
        const ALPHABET: &[char] = &['a', 'я', '字', ' ', ' ', '\n', '\r', '.', '!', '?', '。', '…', '\t'];
        // Deterministic LCG: reproducible inputs mixing every cut rule.
        let mut seed: u64 = 0x5eed;
        for case in 0..300_usize {
            let len = case % 97;
            let text: String = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    let index = usize::try_from(seed >> 33).unwrap_or(0) % ALPHABET.len();
                    ALPHABET[index]
                })
                .collect();
            for max_chars in [1, 2, 3, 7, 16] {
                round_trip(&text, max_chars);
            }
        }
    }

    /// `split_for_request_in` under `Utf16`: round trip, and every chunk within the limit in
    /// UTF-16 code units (limits of at least 2, so an astral char always fits).
    fn round_trip_utf16(text: &str, max_size: usize) -> Vec<RequestChunk> {
        let chunks = split_for_request_in(text, max_size, SizeUnit::Utf16);
        assert_eq!(join_chunks(chunks.iter().map(|chunk| (chunk.text.as_str(), chunk.separator.as_str()))), text, "round trip");
        for chunk in &chunks {
            assert!(SizeUnit::Utf16.measure(&chunk.text) <= max_size, "chunk over the limit: {:?}", chunk.text);
        }
        chunks
    }

    #[test]
    fn sizes_are_measured_in_the_requested_unit() {
        assert_eq!(SizeUnit::Chars.measure("a😀я"), 3);
        assert_eq!(SizeUnit::Utf16.measure("a😀я"), 4);
        // Four emoji are 4 chars but 8 UTF-16 units: one chunk by chars, cut by UTF-16.
        let emoji = "😀😀😀😀";
        assert_eq!(split_for_request(emoji, 4).len(), 1);
        assert_eq!(texts(&round_trip_utf16(emoji, 4)), ["😀😀", "😀😀"]);
        // An odd limit never splits a surrogate pair: the window stops before the char that
        // would cross it.
        assert_eq!(texts(&round_trip_utf16(emoji, 5)), ["😀😀", "😀😀"]);
        // "😀😀 cd" is 7 units: the cut falls at the last space inside the window.
        assert_eq!(texts(&round_trip_utf16("ab 😀😀 cd", 6)), ["ab", "😀😀", "cd"]);
        // A single char wider than the limit is a chunk of its own (never an empty chunk).
        assert_eq!(texts(&split_for_request_in("😀😀", 1, SizeUnit::Utf16)), ["😀", "😀"]);
    }

    #[test]
    fn generated_texts_round_trip_within_a_utf16_limit() {
        const ALPHABET: &[char] = &['a', 'я', '😀', '𠀋', ' ', '\n', '.', '。', '!'];
        let mut seed: u64 = 0xface;
        for case in 0..300_usize {
            let len = case % 61;
            let text: String = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    let index = usize::try_from(seed >> 33).unwrap_or(0) % ALPHABET.len();
                    ALPHABET[index]
                })
                .collect();
            for max_size in [2, 3, 5, 9, 16] {
                round_trip_utf16(&text, max_size);
            }
        }
    }
}
