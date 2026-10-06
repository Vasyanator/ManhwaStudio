/*
File: crates/ms-ai-api/src/structured.rs

Purpose:
Reading a model's answer when it does not follow the requested format exactly: removal of inline
reasoning blocks, tolerant extraction of the JSON value a prompt asked for (inside prose or code
fences, with a few safe syntax repairs, and salvage of the complete elements of a cut-off or
broken array), clean-up of a plain-text answer, and the generic pieces of provider structured
output (a strict object schema, the `genai` response format, and the recognizer of a provider
rejecting that format). Pure and GUI-free; everything except `json_spec_format` is
target-neutral.

Key items:
- strip_think_blocks()         : plain-text answers; removes every `<think>…</think>` block, a
                                 leading reasoning tail closed by a bare `</think>`, and a
                                 dangling unclosed leading block.
- strip_leading_think_blocks() : structured answers; removes only reasoning that PRECEDES the
                                 answer, so text inside a JSON string is never touched.
- JsonShape / extract_json()   : every usable JSON value of the expected shape in an answer, as
                                 ranked `JsonCandidate`s (`JsonExtraction::best_by` lets the
                                 consumer pick the one that validates best), or the salvaged
                                 complete elements of a cut-off / broken array with its `defect`;
                                 `JsonExtractError` says what was wrong, in English for a repair
                                 prompt (`model_feedback`).
- clean_plain_text()           : strips a code fence wrapping the WHOLE answer.
- strict_object_schema()       : an object schema whose every property is required and that
                                 allows no other property (what strict structured output demands).
- json_spec_format()           : native only; the `genai` `ChatResponseFormat::JsonSpec` for a schema.
- response_format_rejected()   : whether a provider error says it refused the requested
                                 structured-output format.

Notes:
Repairs are limited to what cannot change the meaning of a value: trailing commas, `//` and
`/* */` comments outside strings, typographic double quotes used as string delimiters, and raw
control characters inside strings. Single-quoted strings, unquoted keys and the missing end of a
cut-off value are NOT repaired; such an element is dropped and the consumer may ask the model
again for it. Prompts and repair texts are English model input, never UI text.
*/

use serde_json::{Map, Value};

use crate::quota::is_probable_quota_or_limit_error;

/// Opening / closing typographic double quotes a model may use instead of `"`.
const TYPOGRAPHIC_QUOTES: [char; 4] = ['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'];
/// Upper bound of `[` / `{` positions tried per text piece, so a pathological answer full of
/// brackets cannot make the extraction quadratic in practice.
const MAX_CANDIDATES_PER_PIECE: usize = 256;
const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Removes inline reasoning from a PLAIN-TEXT model answer and returns the rest, trimmed.
///
/// Removed, matched ASCII case-insensitively: every closed `<think>…</think>` block wherever it
/// is; everything up to a bare `</think>` that has no opening tag before it (chat templates that
/// put `<think>` into the prompt make the model's output start with the reasoning itself); and a
/// `<think>` block that is never closed when nothing but whitespace precedes it (the model spent
/// its whole output reasoning, so no answer is left). An unclosed `<think>` after real text is
/// kept, as it cannot be told apart from content. A structured (JSON) answer uses
/// `strip_leading_think_blocks` instead, which never touches the answer itself.
#[must_use]
pub fn strip_think_blocks(text: &str) -> String {
    let mut rest = text;
    if let Some(close) = find_ascii_ci(rest, THINK_CLOSE)
        && find_ascii_ci(&rest[..close], THINK_OPEN).is_none()
    {
        rest = &rest[close + THINK_CLOSE.len()..];
    }
    let mut out = String::with_capacity(rest.len());
    loop {
        let Some(open) = find_ascii_ci(rest, THINK_OPEN) else {
            out.push_str(rest);
            break;
        };
        let after_open = &rest[open + THINK_OPEN.len()..];
        if let Some(close) = find_ascii_ci(after_open, THINK_CLOSE) {
            out.push_str(&rest[..open]);
            rest = &after_open[close + THINK_CLOSE.len()..];
            continue;
        }
        // Unclosed block: dropped only when it leads the answer (see the contract above).
        if out.trim().is_empty() && rest[..open].trim().is_empty() {
            out.clear();
        } else {
            out.push_str(rest);
        }
        break;
    }
    out.trim().to_string()
}

/// Removes the reasoning that PRECEDES a structured answer and returns the rest, trimmed:
/// a reasoning tail closed by a bare `</think>` when no `<think>`, `[` or `{` comes before that
/// tag, then every `<think>…</think>` block at the start (only whitespace between them), and a
/// leading `<think>` that never closes (nothing is left then). Tags anywhere later — e.g. inside
/// a translation string of the JSON answer — are kept verbatim.
#[must_use]
pub fn strip_leading_think_blocks(text: &str) -> String {
    let mut rest = text;
    if let Some(close) = find_ascii_ci(rest, THINK_CLOSE) {
        let before = &rest[..close];
        if find_ascii_ci(before, THINK_OPEN).is_none() && !before.contains(['[', '{']) {
            rest = &rest[close + THINK_CLOSE.len()..];
        }
    }
    loop {
        let trimmed = rest.trim_start();
        if !trimmed.as_bytes().get(..THINK_OPEN.len()).is_some_and(|head| head.eq_ignore_ascii_case(THINK_OPEN.as_bytes())) {
            return trimmed.trim_end().to_string();
        }
        let after_open = &trimmed[THINK_OPEN.len()..];
        match find_ascii_ci(after_open, THINK_CLOSE) {
            Some(close) => rest = &after_open[close + THINK_CLOSE.len()..],
            None => return String::new(),
        }
    }
}

/// Byte offset of the first ASCII case-insensitive occurrence of `needle` (ASCII) in `haystack`.
/// The match starts at an ASCII byte, so the offset is always a char boundary.
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack.as_bytes().windows(needle.len()).position(|window| window.eq_ignore_ascii_case(needle))
}

/// The top-level kind of JSON value an answer is expected to contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonShape {
    /// `[ … ]`
    Array,
    /// `{ … }`
    Object,
}

impl JsonShape {
    /// The shape of `value`, `None` for a scalar.
    #[must_use]
    pub fn of(value: &Value) -> Option<Self> {
        match value {
            Value::Array(_) => Some(Self::Array),
            Value::Object(_) => Some(Self::Object),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
        }
    }
}

/// Why no (complete) JSON value could be taken from an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonExtractError {
    /// The answer contains no `[` or `{` at all.
    NotFound,
    /// A JSON value starts but the text ends before it does (typically an answer cut off
    /// mid-way).
    Unterminated,
    /// Bracketed spans exist, but none is valid JSON even after the safe repairs (including
    /// brackets that do not match); `detail` is the first problem found.
    Invalid {
        /// The parser's message, or "mismatched brackets" (English, technical).
        detail: String,
    },
}

impl JsonExtractError {
    /// One English sentence describing the problem, for the repair message sent to the model.
    #[must_use]
    pub fn model_feedback(&self) -> String {
        match self {
            Self::NotFound => "The answer contained no JSON.".to_string(),
            Self::Unterminated => "The JSON in the answer was cut off before it ended.".to_string(),
            Self::Invalid { detail } => format!("The JSON in the answer could not be parsed ({detail})."),
        }
    }
}

/// Where in the answer a candidate was found; a later variant is the more deliberate answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSource {
    /// Plain text outside any code fence.
    Text,
    /// A code fence without a `json` info string.
    Fence,
    /// A code fence tagged `json` (any case).
    JsonFence,
}

/// One usable JSON value found in an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonCandidate {
    pub value: Value,
    pub source: CandidateSource,
}

/// What `extract_json` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonExtraction {
    /// Usable values in text order, never empty: every complete top-level value of the expected
    /// shape; otherwise the salvaged elements of a cut-off / broken array (as one array, see
    /// `defect`); otherwise the first complete value of the other shape.
    pub candidates: Vec<JsonCandidate>,
    /// `Some` when the candidates are salvaged elements: why the array itself was unusable
    /// (`Unterminated` for a cut-off array, `Invalid` for a broken one).
    pub defect: Option<JsonExtractError>,
}

impl JsonExtraction {
    /// The candidate with the highest `score` (the consumer's measure of how well a value
    /// answers the request, e.g. how many requested ids it validly answers). Ties go to the
    /// more deliberate source (`CandidateSource` order), then to the LATER candidate, so a
    /// final answer beats an earlier draft of equal quality.
    /// `None` only for an empty candidate list, which `extract_json` never returns.
    pub fn best_by<F: FnMut(&Value) -> usize>(&self, mut score: F) -> Option<&Value> {
        self.candidates.iter().enumerate().max_by_key(|(index, candidate)| (score(&candidate.value), candidate.source, *index)).map(|(_, candidate)| &candidate.value)
    }
}

/// Finds the JSON an answer carries, tolerating prose around it, code fences and the safe
/// repairs listed in the file header.
///
/// The answer is cut into pieces: every code-fence body (three backticks; tagged `json` or
/// not) and the text outside the fences. In each piece bracket positions are tried left to
/// right; a span is balanced with a string- and comment-aware scan, parsed as is, and parsed
/// again after repair when that fails. Every complete value of shape `expected` becomes a
/// candidate (values nested inside a found value are not searched separately). When there is
/// none, the complete elements of the first array that is cut off (text ends inside it) or
/// broken (an element is not valid JSON, or brackets do not match) are salvaged into one array
/// candidate with `defect` set — the consumer applies them and asks only for the rest. Only
/// then is the first complete value of the other shape returned (e.g. `{"items": [...]}` when an
/// array was expected), so the caller must check the shape.
///
/// # Errors
/// When nothing usable exists: `Unterminated` if some value never ended, otherwise `Invalid`
/// if spans exist but none parses (or brackets do not match), otherwise `NotFound`.
pub fn extract_json(text: &str, expected: JsonShape) -> Result<JsonExtraction, JsonExtractError> {
    let mut scan = Scan::default();
    for (piece, source) in pieces(text) {
        scan.piece(piece, source, expected);
    }
    if !scan.expected.is_empty() {
        return Ok(JsonExtraction { candidates: scan.expected, defect: None });
    }
    if let Some((candidate, defect)) = scan.salvaged {
        return Ok(JsonExtraction { candidates: vec![candidate], defect: Some(defect) });
    }
    if let Some(candidate) = scan.other {
        return Ok(JsonExtraction { candidates: vec![candidate], defect: None });
    }
    if scan.unterminated {
        return Err(JsonExtractError::Unterminated);
    }
    match scan.first_error {
        Some(detail) => Err(JsonExtractError::Invalid { detail }),
        None => Err(JsonExtractError::NotFound),
    }
}

/// What the piece scans of one `extract_json` call found.
#[derive(Debug, Default)]
struct Scan {
    /// Complete values of the expected shape, in text order.
    expected: Vec<JsonCandidate>,
    /// Salvaged elements of the first cut-off / broken array, with its defect.
    salvaged: Option<(JsonCandidate, JsonExtractError)>,
    /// First complete value of the other shape outside any cut-off value.
    other: Option<JsonCandidate>,
    /// Some span never ended.
    unterminated: bool,
    /// First parse / bracket problem of a complete span.
    first_error: Option<String>,
}

impl Scan {
    /// Scans one piece of the answer.
    fn piece(&mut self, piece: &str, source: CandidateSource, expected: JsonShape) {
        let mut position = 0;
        let mut inside_unterminated = false;
        for _ in 0..MAX_CANDIDATES_PER_PIECE {
            let Some(offset) = piece[position..].find(['[', '{']) else {
                return;
            };
            let start = position + offset;
            let is_array = piece[start..].starts_with('[');
            match balanced_span(piece, start) {
                Span::Closed { end, repaired } => match serde_json::from_str::<Value>(&piece[start..end]).or_else(|_| serde_json::from_str::<Value>(&repaired)) {
                    Ok(value) if JsonShape::of(&value) == Some(expected) => {
                        self.expected.push(JsonCandidate { value, source });
                        position = end;
                    }
                    Ok(value) => {
                        if !inside_unterminated && self.other.is_none() {
                            self.other = Some(JsonCandidate { value, source });
                        }
                        // Values nested in a complete value of the other shape belong to it.
                        position = end;
                    }
                    Err(error) => {
                        self.first_error.get_or_insert_with(|| error.to_string());
                        let salvaged = is_array && self.salvage(piece, start, source, JsonExtractError::Invalid { detail: error.to_string() });
                        // A salvaged array is consumed whole; anything else may still hold JSON.
                        position = if salvaged { end } else { start + 1 };
                    }
                },
                Span::Mismatch => {
                    self.first_error.get_or_insert_with(|| "mismatched brackets".to_string());
                    if is_array {
                        self.salvage(piece, start, source, JsonExtractError::Invalid { detail: "mismatched brackets".to_string() });
                    }
                    position = start + 1;
                }
                Span::EndOfText => {
                    self.unterminated = true;
                    if is_array && self.salvage(piece, start, source, JsonExtractError::Unterminated) {
                        // Everything after the opener is inside the cut-off array.
                        return;
                    }
                    inside_unterminated = true;
                    position = start + 1;
                }
            }
        }
    }

    /// Records the complete elements of the array at `start` (when it has any and nothing was
    /// salvaged before); returns whether it had any.
    fn salvage(&mut self, piece: &str, start: usize, source: CandidateSource, defect: JsonExtractError) -> bool {
        let elements = salvage_array_elements(piece, start);
        if elements.is_empty() {
            return false;
        }
        if self.salvaged.is_none() {
            self.salvaged = Some((JsonCandidate { value: Value::Array(elements), source }, defect));
        }
        true
    }
}

/// The complete, valid elements of the array whose `[` is at byte `start`, in order: every
/// object / array element that balances and parses (a broken element is skipped), up to the
/// first element that never ends, does not balance or is not a container (MT answers are
/// arrays of objects).
fn salvage_array_elements(text: &str, start: usize) -> Vec<Value> {
    let mut elements = Vec::new();
    let mut position = start + 1;
    loop {
        position = skip_separators(text, position);
        if !text[position..].starts_with(['{', '[']) {
            return elements;
        }
        let Span::Closed { end, repaired } = balanced_span(text, position) else {
            return elements;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&text[position..end]).or_else(|_| serde_json::from_str::<Value>(&repaired)) {
            elements.push(value);
        }
        position = end;
    }
}

/// The byte offset of the first character from `position` on that is not whitespace, a comma
/// or inside a comment.
fn skip_separators(text: &str, position: usize) -> usize {
    let chars: Vec<(usize, char)> = text[position..].char_indices().map(|(offset, c)| (position + offset, c)).collect();
    let mut index = 0;
    while let Some(&(_, c)) = chars.get(index) {
        if c.is_whitespace() || c == ',' {
            index += 1;
        } else if c == '/' && matches!(chars.get(index + 1), Some(&(_, '/' | '*'))) {
            index = skip_comment(&chars, index);
        } else {
            break;
        }
    }
    chars.get(index).map_or(text.len(), |&(byte, _)| byte)
}

/// The pieces of `text` in order: each code-fence body with its source, and the text around
/// the fences as `CandidateSource::Text`. A fence's first line is its info string (e.g. `json`)
/// when it is a bare word; an unclosed fence runs to the end of the text.
fn pieces(text: &str) -> Vec<(&str, CandidateSource)> {
    const FENCE: &str = "```";
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(FENCE) {
        pieces.push((&rest[..open], CandidateSource::Text));
        let after = &rest[open + FENCE.len()..];
        let (body_start, source) = match after.find('\n') {
            Some(newline) if after[..newline].trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => {
                let source = if after[..newline].trim().eq_ignore_ascii_case("json") { CandidateSource::JsonFence } else { CandidateSource::Fence };
                (newline + 1, source)
            }
            _ => (0, CandidateSource::Fence),
        };
        let body = &after[body_start..];
        let Some(close) = body.find(FENCE) else {
            pieces.push((body, source));
            return pieces;
        };
        pieces.push((&body[..close], source));
        rest = &body[close + FENCE.len()..];
    }
    pieces.push((rest, CandidateSource::Text));
    pieces
}

/// Scanner state of `balanced_span`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    /// Outside strings.
    Code,
    /// Inside a string opened by `"` (`typographic == false`) or by a typographic quote.
    Str { typographic: bool },
}

/// How the bracketed value at a position ends.
#[derive(Debug)]
enum Span {
    /// It closes at byte `end` (exclusive); `repaired` is its text after the safe repairs.
    Closed { end: usize, repaired: String },
    /// The text ends inside it (in code or in a `"` string): a cut-off value.
    EndOfText,
    /// A closing bracket does not match, or the text ends inside a typographic-quote string
    /// (no JSON writer leaves one open): broken, not cut off.
    Mismatch,
}

/// Scans the bracketed value starting at byte `start` (a `[` or `{`), repairing as it goes
/// (comments removed, trailing commas dropped, typographic delimiters turned into `"`, control
/// characters inside strings escaped).
fn balanced_span(text: &str, start: usize) -> Span {
    let chars: Vec<(usize, char)> = text[start..].char_indices().map(|(offset, c)| (start + offset, c)).collect();
    let mut out = String::with_capacity(chars.len());
    let mut closers: Vec<char> = Vec::new();
    let mut mode = ScanMode::Code;
    let mut index = 0;
    while let Some(&(byte, c)) = chars.get(index) {
        match mode {
            ScanMode::Code => match c {
                '"' => {
                    out.push('"');
                    mode = ScanMode::Str { typographic: false };
                }
                c if TYPOGRAPHIC_QUOTES.contains(&c) => {
                    out.push('"');
                    mode = ScanMode::Str { typographic: true };
                }
                '/' if matches!(chars.get(index + 1), Some(&(_, '/' | '*'))) => {
                    index = skip_comment(&chars, index);
                    continue;
                }
                '[' => {
                    closers.push(']');
                    out.push(c);
                }
                '{' => {
                    closers.push('}');
                    out.push(c);
                }
                ']' | '}' => {
                    if closers.pop() != Some(c) {
                        return Span::Mismatch;
                    }
                    out.push(c);
                    if closers.is_empty() {
                        return Span::Closed { end: byte + c.len_utf8(), repaired: out };
                    }
                }
                // A comma directly before a closing bracket is a trailing comma: dropped.
                ',' if matches!(next_significant(&chars, index + 1), Some(']' | '}')) => {}
                _ => out.push(c),
            },
            ScanMode::Str { typographic } => match c {
                '\\' => {
                    out.push('\\');
                    if let Some(&(_, escaped)) = chars.get(index + 1) {
                        out.push(escaped);
                        index += 2;
                        continue;
                    }
                }
                '"' if !typographic => {
                    out.push('"');
                    mode = ScanMode::Code;
                }
                // In a typographic string a quote only closes it where JSON syntax continues;
                // elsewhere it is part of the text (e.g. nested quotations).
                c if typographic && (c == '"' || TYPOGRAPHIC_QUOTES.contains(&c)) && closes_string(&chars, index + 1) => {
                    out.push('"');
                    mode = ScanMode::Code;
                }
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if u32::from(c) < 0x20 => push_unicode_escape(&mut out, c),
                _ => out.push(c),
            },
        }
        index += 1;
    }
    if mode == (ScanMode::Str { typographic: true }) { Span::Mismatch } else { Span::EndOfText }
}

/// Appends the JSON escape `\u00XX` of a control character (`c` < U+0020).
fn push_unicode_escape(out: &mut String, c: char) {
    let code = u32::from(c);
    out.push_str("\\u00");
    for nibble in [(code >> 4) & 0xF, code & 0xF] {
        // A nibble is always a valid hex digit, so `from_digit` never returns `None` here.
        out.push(char::from_digit(nibble, 16).unwrap_or('0'));
    }
}

/// Index just past the `//` line comment or `/* */` block comment starting at `index` (an
/// unclosed block comment runs to the end).
fn skip_comment(chars: &[(usize, char)], index: usize) -> usize {
    let block = matches!(chars.get(index + 1), Some(&(_, '*')));
    let mut cursor = index + 2;
    while let Some(&(_, c)) = chars.get(cursor) {
        if block {
            if c == '*' && matches!(chars.get(cursor + 1), Some(&(_, '/'))) {
                return cursor + 2;
            }
        } else if c == '\n' {
            return cursor;
        }
        cursor += 1;
    }
    cursor
}

/// The next character from `index` on that is neither whitespace nor inside a comment.
fn next_significant(chars: &[(usize, char)], mut index: usize) -> Option<char> {
    while let Some(&(_, c)) = chars.get(index) {
        if c.is_whitespace() {
            index += 1;
        } else if c == '/' && matches!(chars.get(index + 1), Some(&(_, '/' | '*'))) {
            index = skip_comment(chars, index);
        } else {
            return Some(c);
        }
    }
    None
}

/// `true` when the text from `index` continues as JSON syntax after a string (`:` `,` `]` `}`)
/// or ends.
fn closes_string(chars: &[(usize, char)], index: usize) -> bool {
    matches!(next_significant(chars, index), None | Some(':' | ',' | ']' | '}'))
}

/// Cleans a plain-text answer: trims it and strips a code fence (three backticks) that wraps
/// the WHOLE answer, with its info string. Quotes are never stripped (a bubble may really show
/// a quotation); text with any other surrounding (prose, several fences) is returned trimmed but
/// unchanged, as it cannot be told apart from content.
#[must_use]
pub fn clean_plain_text(text: &str) -> String {
    const FENCE: &str = "```";
    let cleaned = text.trim();
    if let Some(inner) = cleaned.strip_prefix(FENCE).and_then(|inner| inner.strip_suffix(FENCE))
        && !inner.contains(FENCE)
    {
        let body = match inner.find('\n') {
            Some(newline) if inner[..newline].trim().chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => &inner[newline + 1..],
            _ => inner,
        };
        return body.trim().to_string();
    }
    cleaned.to_string()
}

/// A JSON schema of an object with exactly `properties` (name, schema), all of them required
/// and no other property allowed: the form strict structured output (`OpenAI` `strict: true`,
/// Anthropic `output_config.format`) accepts. Optional fields must be expressed by the caller
/// (e.g. an `anyOf` of item shapes), never by leaving a property out of `required`.
#[must_use]
pub fn strict_object_schema(properties: &[(&str, Value)]) -> Value {
    let mut map = Map::new();
    let mut required = Vec::with_capacity(properties.len());
    for (name, schema) in properties {
        map.insert((*name).to_string(), schema.clone());
        required.push(Value::String((*name).to_string()));
    }
    serde_json::json!({ "type": "object", "properties": map, "required": required, "additionalProperties": false })
}

/// The `genai` structured-output format for `schema` (`ChatResponseFormat::JsonSpec`), to set
/// on the request's `ChatOptions::response_format`. `name` is reduced to the characters `OpenAI`
/// accepts in a schema name (`A-Z a-z 0-9 _ -`, others become `_`; empty becomes `response`).
/// Providers without structured output either ignore it or reject the request, which
/// `validated::exec_chat_validated` answers by sending the request again without it.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn json_spec_format(name: &str, schema: Value) -> genai::chat::ChatResponseFormat {
    let mut safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if safe.is_empty() {
        safe = "response".to_string();
    }
    genai::chat::ChatResponseFormat::JsonSpec(genai::chat::JsonSpec::new(safe, schema))
}

/// `true` when a provider error (its text, any case) says the request's structured-output
/// format was refused: it names the format parameter (`response_format`, `json_schema` /
/// "json schema", Gemini's `responseJsonSchema` / `responseSchema` / `responseMimeType`,
/// Anthropic's `output_config`) or "structured output". Never for a quota / rate-limit / billing
/// error (`is_probable_quota_or_limit_error`) or an authentication error (401 / 403 wording),
/// which some proxies answer while echoing the request's parameters. A generic schema
/// complaint without those words does not count. Meant only for an error that came before any
/// output of a request that DID carry a response format.
#[must_use]
pub fn response_format_rejected(error_text: &str) -> bool {
    const FORMAT_MARKERS: [&str; 12] = ["response_format", "json_schema", "json schema", "response_schema", "responseschema", "responsejsonschema", "response_mime_type", "responsemimetype", "output_config", "structured output", "structured_output", "structured-output"];
    const AUTH_MARKERS: [&str; 10] = ["status: 401", "status: 403", "status code: 401", "status code: 403", "401 unauthorized", "403 forbidden", "unauthorized", "forbidden", "invalid api key", "authentication"];
    let text = error_text.to_lowercase();
    if is_probable_quota_or_limit_error(&text) || AUTH_MARKERS.iter().any(|marker| text.contains(marker)) {
        return false;
    }
    FORMAT_MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{CandidateSource, JsonCandidate, JsonExtractError, JsonExtraction, JsonShape, clean_plain_text, extract_json, response_format_rejected, strict_object_schema, strip_leading_think_blocks, strip_think_blocks};

    fn array(text: &str) -> Result<JsonExtraction, JsonExtractError> {
        extract_json(text, JsonShape::Array)
    }

    /// The single candidate of a successful extraction (panics with context otherwise).
    fn only(text: &str, shape: JsonShape) -> Value {
        let extraction = extract_json(text, shape).unwrap_or_else(|error| panic!("{text:?} gave {error:?}"));
        assert_eq!(extraction.candidates.len(), 1, "{text:?} gave {extraction:?}");
        assert_eq!(extraction.defect, None);
        extraction.candidates[0].value.clone()
    }

    /// The number of elements of a candidate that are objects with an `id` — the way a
    /// consumer like MT scores candidates.
    fn id_score(value: &Value) -> usize {
        value.as_array().map_or(0, |items| items.iter().filter(|item| item.get("id").is_some()).count())
    }

    #[test]
    fn think_blocks_are_removed_everywhere_from_plain_text() {
        assert_eq!(strip_think_blocks("<think>plan</think>\n[1]"), "[1]");
        assert_eq!(strip_think_blocks("a <THINK>x</Think> b <think>y</think> c"), "a  b  c");
        assert_eq!(strip_think_blocks("  plain answer \n"), "plain answer");
        assert_eq!(strip_think_blocks(""), "");
        assert_eq!(strip_think_blocks("Okay, the user wants text.\n</think>\n\nHELLO"), "HELLO");
        assert_eq!(strip_think_blocks("  <think>still reasoning when the limit hit"), "");
        assert_eq!(strip_think_blocks("answer <think>tail"), "answer <think>tail");
        assert_eq!(strip_think_blocks("<think>думаю…</think> 안녕 — привет"), "안녕 — привет");
    }

    #[test]
    fn leading_think_stripping_removes_only_reasoning_before_the_answer() {
        assert_eq!(strip_leading_think_blocks("<think>a</think>\n <THINK>b</think> [1]"), "[1]");
        assert_eq!(strip_leading_think_blocks("Okay, the user wants JSON.\n</think>\n\n[{\"id\":1}]"), "[{\"id\":1}]");
        assert_eq!(strip_leading_think_blocks("<think>never closed"), "");
        let inside = r#"[{"id":1,"translation":"<think>a</think> b"}]"#;
        assert_eq!(strip_leading_think_blocks(inside), inside);
        let bare = r#"[{"id":1,"translation":"x </think> y"}]"#;
        assert_eq!(strip_leading_think_blocks(bare), bare);
        assert_eq!(strip_leading_think_blocks("<think>p</think>[{\"id\":1,\"translation\":\"<think>a</think> b\"}]"), r#"[{"id":1,"translation":"<think>a</think> b"}]"#);
    }

    #[test]
    fn plain_and_fenced_json_is_extracted() {
        assert_eq!(only(r#"[{"id":1,"translation":"a"}]"#, JsonShape::Array), json!([{"id":1,"translation":"a"}]));
        assert_eq!(only("```json\n[{\"id\":1}]\n```", JsonShape::Array), json!([{"id":1}]));
        assert_eq!(only("```\n[2]\n```", JsonShape::Array), json!([2]));
    }

    #[test]
    fn fences_are_tagged_by_their_info_string() {
        let extraction = array("```json\n[1]\n```\n```\n[2]\n```\n[3]").expect("three arrays");
        let sources: Vec<CandidateSource> = extraction.candidates.iter().map(|candidate| candidate.source).collect();
        assert_eq!(sources, vec![CandidateSource::JsonFence, CandidateSource::Fence, CandidateSource::Text]);
        assert_eq!(extraction.best_by(|_| 0), Some(&json!([1])), "on equal score the json fence wins");
    }

    #[test]
    fn prose_around_the_json_is_ignored() {
        let answer = "Sure! Here is the translation [as requested]:\n```json\n[{\"id\": 3, \"translation\": \"Привет [шёпотом]\"}]\n```\nLet me know if you need anything else.";
        let extraction = array(answer).expect("found");
        assert_eq!(extraction.best_by(id_score), Some(&json!([{"id":3,"translation":"Привет [шёпотом]"}])));
        let bare = "Here you go: [{\"id\":4,\"translation\":\"ok\"}] Hope it helps {:)}";
        assert_eq!(only(bare, JsonShape::Array), json!([{"id":4,"translation":"ok"}]));
    }

    #[test]
    fn a_stray_bracket_in_prose_does_not_win_over_the_answer() {
        let answer = "Translated [2] items:\n[{\"id\":1,\"translation\":\"a\"},{\"id\":2,\"translation\":\"b\"}]";
        let extraction = array(answer).expect("found");
        assert_eq!(extraction.candidates.len(), 2);
        assert_eq!(extraction.best_by(id_score), Some(&json!([{"id":1,"translation":"a"},{"id":2,"translation":"b"}])));
    }

    #[test]
    fn a_final_array_beats_an_equally_good_draft() {
        let answer = "Draft: [{\"id\":1,\"translation\":\"X\"}]\nFinal: [{\"id\":1,\"translation\":\"Y\"}]";
        assert_eq!(array(answer).expect("found").best_by(id_score), Some(&json!([{"id":1,"translation":"Y"}])));
    }

    #[test]
    fn brackets_and_quotes_inside_strings_do_not_break_balancing() {
        let answer = r#"[{"id":1,"translation":"He said \"[no]\" and left }"}]"#;
        assert_eq!(only(answer, JsonShape::Array), json!([{"id":1,"translation":"He said \"[no]\" and left }"}]));
        assert_eq!(only(r#"[{"id":1,"translation":"a // b ,] “q”"}]"#, JsonShape::Array), json!([{"id":1,"translation":"a // b ,] “q”"}]));
        assert_eq!(only(r#"[{"id":1,"translation":"<think> is a tag"}]"#, JsonShape::Array), json!([{"id":1,"translation":"<think> is a tag"}]));
    }

    #[test]
    fn trailing_commas_and_comments_are_repaired() {
        let answer = "[\n  {\"id\": 1, \"translation\": \"a\",}, // first\n  /* second */ {\"id\": 2, \"translation\": \"b // not a comment\"},\n]";
        assert_eq!(only(answer, JsonShape::Array), json!([{"id":1,"translation":"a"},{"id":2,"translation":"b // not a comment"}]));
        assert_eq!(only("[{\"id\":1,\"translation\":\"он „сказал“, да\",},]", JsonShape::Array), json!([{"id":1,"translation":"он „сказал“, да"}]));
        assert_eq!(only("[{\"id\":1,\"translation\":\"a\\\\\"},]", JsonShape::Array), json!([{"id":1,"translation":"a\\"}]));
    }

    #[test]
    fn typographic_delimiters_are_repaired_but_quotes_in_text_are_kept() {
        let answer = "[{\u{201C}id\u{201D}: 1, \u{201C}translation\u{201D}: \u{201C}Он сказал \u{201E}нет\u{201C} и ушёл\u{201D}}]";
        assert_eq!(only(answer, JsonShape::Array), json!([{"id":1,"translation":"Он сказал \u{201E}нет\u{201C} и ушёл"}]));
        let inside_ascii = r#"[{"id":2,"translation":"«Да» и “нет”"}]"#;
        assert_eq!(only(inside_ascii, JsonShape::Array), json!([{"id":2,"translation":"«Да» и “нет”"}]));
        assert_eq!(only("[{“id”: 1, “translation”: “see http://x”,}]", JsonShape::Array), json!([{"id":1,"translation":"see http://x"}]));
    }

    #[test]
    fn raw_newlines_inside_strings_are_escaped() {
        assert_eq!(only("[{\"id\":1,\"translation\":\"line one\nline two\"}]", JsonShape::Array), json!([{"id":1,"translation":"line one\nline two"}]));
    }

    #[test]
    fn a_wrapping_object_is_returned_when_no_array_stands_alone() {
        let answer = r#"{"items":[{"id":1,"translation":"a"}]}"#;
        assert_eq!(only(answer, JsonShape::Array), json!({"items":[{"id":1,"translation":"a"}]}));
        assert_eq!(only(answer, JsonShape::Object), json!({"items":[{"id":1,"translation":"a"}]}));
    }

    #[test]
    fn a_single_object_answer_is_not_split_into_its_inner_arrays() {
        let answer = r#"{"id":7,"areas":[{"original_text":"A","translation":"А"},{"original_text":"B","translation":"Б"}]}"#;
        assert_eq!(only(answer, JsonShape::Array), json!({"id":7,"areas":[{"original_text":"A","translation":"А"},{"original_text":"B","translation":"Б"}]}));
    }

    #[test]
    fn object_shape_prefers_the_object() {
        let answer = "Result:\n{\"id\": 10, \"original_text\": \"BOOM\", \"translation\": \"БУМ\"}";
        assert_eq!(only(answer, JsonShape::Object), json!({"id":10,"original_text":"BOOM","translation":"БУМ"}));
    }

    #[test]
    fn a_cut_off_array_salvages_its_complete_elements() {
        let answer = r#"[{"id":1,"translation":"a"},{"id":2,"translation":"b"},{"id":3,"transl"#;
        let extraction = array(answer).expect("salvaged");
        assert_eq!(extraction.defect, Some(JsonExtractError::Unterminated));
        assert_eq!(extraction.candidates, vec![JsonCandidate { value: json!([{"id":1,"translation":"a"},{"id":2,"translation":"b"}]), source: CandidateSource::Text }]);
        let in_string = array(r#"[{"id":1,"translation":"pick [1] now"},{"id":2,"#).expect("salvaged");
        assert_eq!(in_string.candidates[0].value, json!([{"id":1,"translation":"pick [1] now"}]));
        let wrapped = extract_json(r#"{"items":[{"id":1,"translation":"a"},{"id":2,"tr"#, JsonShape::Object).expect("salvaged");
        assert_eq!((wrapped.candidates[0].value.clone(), wrapped.defect), (json!([{"id":1,"translation":"a"}]), Some(JsonExtractError::Unterminated)));
    }

    #[test]
    fn a_broken_element_is_skipped_and_the_rest_salvaged() {
        let answer = r#"[{"id":1,"translation":"a"},{"id":2,"translation":'b'},{"id":3,"translation":"c"}]"#;
        let extraction = array(answer).expect("salvaged");
        assert!(matches!(extraction.defect, Some(JsonExtractError::Invalid { .. })), "{extraction:?}");
        assert_eq!(extraction.candidates[0].value, json!([{"id":1,"translation":"a"},{"id":3,"translation":"c"}]));
    }

    #[test]
    fn nothing_usable_in_a_cut_off_answer_is_unterminated() {
        assert_eq!(array("```json\n[{\"id\":1,"), Err(JsonExtractError::Unterminated));
        assert_eq!(extract_json(r#"{"id":10,"original_text":"BO"#, JsonShape::Object), Err(JsonExtractError::Unterminated));
    }

    #[test]
    fn missing_or_broken_json_is_reported() {
        assert_eq!(array("I cannot translate this."), Err(JsonExtractError::NotFound));
        assert!(matches!(array("[{id: 1, translation: 'a'}]"), Err(JsonExtractError::Invalid { .. })));
        assert!(matches!(array("[1 2]"), Err(JsonExtractError::Invalid { .. })));
        assert!(JsonExtractError::Invalid { detail: "x".to_string() }.model_feedback().contains("could not be parsed"));
    }

    #[test]
    fn mismatched_brackets_are_broken_not_cut_off() {
        assert!(matches!(array("[1, 2}"), Err(JsonExtractError::Invalid { .. })));
        assert!(matches!(array("Note (see [a}) none"), Err(JsonExtractError::Invalid { .. })));
        assert!(matches!(array("[“He said \"hi\", then left”"), Err(JsonExtractError::Invalid { .. })));
    }

    #[test]
    fn plain_text_loses_a_whole_answer_fence_only() {
        assert_eq!(clean_plain_text("```text\n안녕하세요\n잘 지내?\n```"), "안녕하세요\n잘 지내?");
        assert_eq!(clean_plain_text("```\nHELLO\n```"), "HELLO");
        assert_eq!(clean_plain_text("  \"WAIT!\"  "), "\"WAIT!\"");
        assert_eq!(clean_plain_text("«Магазин»"), "«Магазин»");
        assert_eq!(clean_plain_text("「ドン」"), "「ドン」");
        assert_eq!(clean_plain_text("Here:\n```\nX\n```"), "Here:\n```\nX\n```");
        assert_eq!(clean_plain_text("쾅 / 쿵"), "쾅 / 쿵");
    }

    #[test]
    fn strict_object_schema_requires_every_property() {
        let schema = strict_object_schema(&[("id", json!({"type": "integer"})), ("translation", json!({"type": "string"}))]);
        assert_eq!(schema, json!({"type": "object", "properties": {"id": {"type": "integer"}, "translation": {"type": "string"}}, "required": ["id", "translation"], "additionalProperties": false}));
    }

    #[test]
    fn recognizes_structured_output_refusals() {
        assert!(response_format_rejected(r#"Status: 400 Bad Request Body: {"error":{"message":"Invalid parameter: 'response_format' of type 'json_schema' is not supported with this model.","param":"response_format"}}"#));
        assert!(response_format_rejected("This response_format type is unavailable now"));
        assert!(response_format_rejected("Invalid JSON payload received. Unknown name \"responseJsonSchema\" at 'generation_config'"));
        assert!(response_format_rejected("output_config.format: Extra inputs are not permitted"));
        assert!(response_format_rejected("Structured outputs are not supported for this model"));
    }

    #[test]
    fn other_errors_are_not_structured_output_refusals() {
        assert!(!response_format_rejected("429 You exceeded your current quota. Request: {\"model\":\"x\",\"response_format\":{...}}"));
        assert!(!response_format_rejected("400 Invalid value for 'messages[2].content': schema validation failed"));
        assert!(!response_format_rejected("Status: 401 Unauthorized, request had response_format json_schema"));
        assert!(!response_format_rejected("Status: 403 Forbidden {\"response_format\":{}}"));
        assert!(!response_format_rejected("Status: 429 Too Many Requests"));
        assert!(!response_format_rejected("Your organization must be verified to stream this model"));
        assert!(!response_format_rejected("The provided schema is not supported"));
        assert!(!response_format_rejected(""));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn json_spec_name_is_sanitized() {
        let format = super::json_spec_format("manga batch/v1", json!({"type": "object"}));
        let genai::chat::ChatResponseFormat::JsonSpec(spec) = format else { panic!("a JsonSpec format was built") };
        assert_eq!(spec.name, "manga_batch_v1");
        let genai::chat::ChatResponseFormat::JsonSpec(spec) = super::json_spec_format("", json!({})) else { panic!("a JsonSpec format was built") };
        assert_eq!(spec.name, "response");
    }
}
