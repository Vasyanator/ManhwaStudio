/*
FILE OVERVIEW: crates/ms-tab-translation/src/ai_mt_response.rs
Reading, validating and merging AI API machine-translation answers, plus the content checks that
mark doubtful translations. Pure (no I/O, no events, no logging); `machine_translation.rs` drives
it from `ms_ai_api::exec_chat_validated`.

Main items:
- `ExpectedKind`: the answer shape one translatable item needs (text / single-area image /
  multi-area image with its area count).
- `AnswerForm`: how the request asked the model to shape the whole answer (flat array, the
  `{"items": [...]}` wrapper of strict structured output, or one object in per-ImageBubble mode).
- `BatchCollector`: the per-request accumulator. `absorb` reads one answer (strips only the
  reasoning BEFORE the answer, `strip_leading_think_blocks`; extracts the JSON candidates with
  `ms_ai_api::structured::extract_json` and takes the one that validly answers the most
  requested ids; a cut-off or broken array still yields its complete elements), validates every
  element against the expected id set and keeps the valid ones (first valid answer per id wins,
  across the first answer and the repair retry); `repair_request` builds the ONE retry that
  re-asks only the ids still missing or invalid; `failure_text` gives each still-missing id its
  localized reason; `history_json` is the canonical answer kept in the batch chat history.
- `AiMtTranslation` / `AiMtArea`: one accepted translation.
- `ContentWarning` / `content_warnings`: MARK-only checks (identical to the source, still in the
  source writing system, absurdly long). They never reject an item and never trigger a retry.
- `strict_answer_schema`: the JSON schema of the answer for provider structured output.

Notes:
- Ids are accepted as JSON integers or numeric strings (`"12"`). Unknown and duplicate ids are
  ignored and reported to the model; ids of context replicas are ignored silently.
- In per-ImageBubble mode (`lenient_single`) an element with a missing or wrong id is still
  accepted for the one target when it has the target's shape (models often echo another id).
- Repair texts are English model input, never UI text; every reason shown to the user is
  localized (`translation.mt.*`).
*/

use std::collections::{HashMap, HashSet};

use ms_ai_api::structured::{JsonExtractError, JsonShape, extract_json, strict_object_schema, strip_leading_think_blocks};
use ms_ai_api::{RepairRequest, RetryReason};
use serde_json::{Value, json};

use crate::machine_translation::{MtImageInput, MtTranslateItem};

/// Most element problems listed in one repair message; the rest are summarized as a count.
const MAX_LISTED_PROBLEMS: usize = 20;
/// A translation longer than this many times its source (and longer than
/// `TOO_LONG_MIN_CHARS`) is marked as suspicious. The source is measured in weighted chars
/// (`CJK_SOURCE_CHAR_WEIGHT`).
const TOO_LONG_RATIO: usize = 4;
/// Weight of one Hangul / kana / Han source char when measuring a source for `TooLong`: one
/// such char carries roughly a word, so a normal CJK -> Latin translation is several times
/// longer in chars than its source.
const CJK_SOURCE_CHAR_WEIGHT: usize = 3;
/// Short translations are never marked as too long, whatever the ratio.
const TOO_LONG_MIN_CHARS: usize = 40;
/// A normalized source text needs at least this many letters before an identical
/// translation is marked (short words, names and sound effects are often kept as is).
const SAME_AS_SOURCE_MIN_LETTERS: usize = 4;

/// The answer shape one translatable item needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExpectedKind {
    /// `{"id", "translation"}`.
    Text,
    /// `{"id", "original_text", "translation"}`: an image bubble with one text area.
    SingleImage,
    /// `{"id", "areas": [{"original_text", "translation"}; area_count]}`.
    MultiImage { area_count: usize },
}

impl ExpectedKind {
    /// The kind of `item` (its image input decides the image kinds).
    pub(crate) fn of(item: &MtTranslateItem) -> Self {
        match item.image.as_ref() {
            None => Self::Text,
            Some(image) if MtImageInput::is_multi_area(image) => Self::MultiImage { area_count: image.areas.len() },
            Some(_) => Self::SingleImage,
        }
    }
}

/// How the request asked the model to shape the whole answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerForm {
    /// A flat JSON array of items (batched mode).
    Array,
    /// `{"items": [...]}` (batched mode with provider structured output, which needs an object
    /// at the top level).
    WrappedArray,
    /// One object for the one target (per-ImageBubble mode).
    Object,
}

/// One accepted translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiMtTranslation {
    pub(crate) bubble_id: i64,
    /// The text the model read (image items; also passed on when a text item echoes it).
    pub(crate) original_text: Option<String>,
    /// Empty for a multi-area image item.
    pub(crate) translation: String,
    /// Per-area results of a multi-area image item, exactly `area_count` of them.
    pub(crate) areas: Vec<AiMtArea>,
}

/// One area of a multi-area image result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiMtArea {
    pub(crate) original_text: String,
    pub(crate) translation: String,
}

/// A defect of one element of an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ElementProblem {
    /// The element (0-based `position`) is not a JSON object.
    NotAnObject { position: usize },
    /// The element has no integer or numeric-string id.
    NoId { position: usize },
    /// The id was not asked for (ignored).
    UnknownId(i64),
    /// The id appears again in the same answer (the first copy was used).
    DuplicateId(i64),
    /// `translation` missing, not a string or empty.
    MissingTranslation(i64),
    /// An image item without the `original_text` read from the image.
    MissingOriginal(i64),
    /// A multi-area image item without an `areas` array.
    MissingAreas(i64),
    /// A multi-area image item with a wrong number of usable areas.
    AreaCount { id: i64, expected: usize, got: usize },
}

impl ElementProblem {
    /// English description for the repair message.
    fn model_text(&self) -> String {
        match self {
            Self::NotAnObject { position } => format!("Array element {} is not a JSON object.", position + 1),
            Self::NoId { position } => format!("Array element {} has no numeric \"id\".", position + 1),
            Self::UnknownId(id) => format!("id {id} was not requested; it was ignored."),
            Self::DuplicateId(id) => format!("id {id} appears more than once; only the first entry was used."),
            Self::MissingTranslation(id) => format!("id {id}: \"translation\" is missing, empty or not a string."),
            Self::MissingOriginal(id) => format!("id {id}: \"original_text\" (the text read from the image) is missing or empty."),
            Self::MissingAreas(id) => format!("id {id}: \"areas\" is missing; this image item needs one {{\"original_text\", \"translation\"}} entry per area."),
            Self::AreaCount { id, expected, got } => format!("id {id}: \"areas\" has {got} usable entries but area_count is {expected}."),
        }
    }
}

/// Why an expected item has no accepted translation (yet).
#[derive(Debug, Clone, PartialEq, Eq)]
enum MissingReason {
    /// The answer did not contain the id.
    NotReturned,
    /// The answer stopped at the output limit before the id.
    Truncated,
    /// No JSON could be read from the answer.
    Unparsable(JsonExtractError),
    /// The JSON was neither an array nor an object holding the items.
    NotAList,
    /// The id's element was invalid.
    Element(ElementProblem),
}

/// What one `BatchCollector::absorb` call found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AbsorbReport {
    /// Ids accepted from this answer, in the batch's order.
    pub(crate) newly_accepted: Vec<i64>,
    /// Elements the answer held (0 when nothing could be read).
    pub(crate) entries: usize,
    /// Defects of individual elements.
    pub(crate) problems: Vec<ElementProblem>,
    /// No JSON value could be read from the answer.
    pub(crate) parse_error: Option<JsonExtractError>,
    /// The JSON value held no item list.
    pub(crate) not_a_list: bool,
    /// The provider cut the answer off at its output limit.
    pub(crate) truncated: bool,
}

impl AbsorbReport {
    /// One line for the run log.
    pub(crate) fn log_summary(&self) -> String {
        let parse = self.parse_error.as_ref().map_or_else(String::new, |error| format!(", parse error: {}", error.model_feedback()));
        format!("{} entries, {} accepted, {} element problems, not_a_list={}, truncated={}{parse}", self.entries, self.newly_accepted.len(), self.problems.len(), self.not_a_list, self.truncated)
    }
}

/// Per-request accumulator of validated translations (see the file header).
#[derive(Debug)]
pub(crate) struct BatchCollector {
    /// Translatable items of the request, in its order.
    expected: Vec<(i64, ExpectedKind)>,
    /// Context replicas of the request; returned anyway, they are ignored silently.
    context_ids: HashSet<i64>,
    accepted: HashMap<i64, AiMtTranslation>,
    /// Latest reason per id that is not accepted.
    missing_reasons: HashMap<i64, MissingReason>,
    form: AnswerForm,
    /// Accept a wrong-id element for the single target (per-ImageBubble mode).
    lenient_single: bool,
}

impl BatchCollector {
    /// A collector for the request built from `items` (context replicas included).
    pub(crate) fn new(items: &[MtTranslateItem], form: AnswerForm, lenient_single: bool) -> Self {
        let expected = items.iter().filter(|item| item.needs_translation).map(|item| (item.bubble_id, ExpectedKind::of(item))).collect();
        let context_ids = items.iter().filter(|item| !item.needs_translation).map(|item| item.bubble_id).collect();
        Self { expected, context_ids, accepted: HashMap::new(), missing_reasons: HashMap::new(), form, lenient_single }
    }

    /// Reads one answer (`text` as received; `truncated` = stopped at the output limit) and
    /// accepts every valid element for an id that has no accepted translation yet.
    pub(crate) fn absorb(&mut self, text: &str, truncated: bool) -> AbsorbReport {
        let mut report = AbsorbReport { truncated, ..AbsorbReport::default() };
        let cleaned = strip_leading_think_blocks(text);
        let preferred = match self.form {
            AnswerForm::Array => JsonShape::Array,
            AnswerForm::WrappedArray | AnswerForm::Object => JsonShape::Object,
        };
        let extracted = extract_json(&cleaned, preferred).and_then(|extraction| {
            // Of several candidates (a stray `[2]` in prose, a draft before the final answer) the
            // one that validly answers the most requested ids wins.
            let value = extraction.best_by(|value| self.score(value)).cloned().ok_or(JsonExtractError::NotFound)?;
            Ok((value, extraction.defect))
        });
        let (value, defect) = match extracted {
            Ok(found) => found,
            Err(error) => {
                let reason = if truncated { MissingReason::Truncated } else { MissingReason::Unparsable(error.clone()) };
                self.mark_missing(&reason);
                report.parse_error = Some(error);
                return report;
            }
        };
        // Salvaged elements of a cut-off / broken array: applied, the defect is still reported.
        report.parse_error.clone_from(&defect);
        let Some(elements) = elements_of(&value) else {
            self.mark_missing(&if truncated { MissingReason::Truncated } else { MissingReason::NotAList });
            report.not_a_list = true;
            return report;
        };
        report.entries = elements.len();
        let mut seen: HashSet<i64> = HashSet::new();
        let mut orphans: Vec<&Value> = Vec::new();
        for (position, element) in elements.into_iter().enumerate() {
            if !element.is_object() {
                report.problems.push(ElementProblem::NotAnObject { position });
                continue;
            }
            let Some(id) = element_id(element) else {
                report.problems.push(ElementProblem::NoId { position });
                orphans.push(element);
                continue;
            };
            if self.context_ids.contains(&id) {
                continue;
            }
            let Some(kind) = self.kind_of(id) else {
                report.problems.push(ElementProblem::UnknownId(id));
                orphans.push(element);
                continue;
            };
            if !seen.insert(id) {
                report.problems.push(ElementProblem::DuplicateId(id));
                continue;
            }
            if self.accepted.contains_key(&id) {
                continue;
            }
            match parse_entry(element, kind, id) {
                Ok(entry) => self.accept(entry, &mut report),
                Err(problem) => {
                    self.missing_reasons.insert(id, MissingReason::Element(problem.clone()));
                    report.problems.push(problem);
                }
            }
        }
        let single_target = match self.expected.as_slice() {
            [(target, kind)] if self.lenient_single => Some((*target, *kind)),
            _ => None,
        };
        if let Some((target, kind)) = single_target
            && !self.accepted.contains_key(&target)
            && let Some(entry) = orphans.into_iter().find_map(|element| parse_entry(element, kind, target).ok())
        {
            self.accept(entry, &mut report);
            seen.insert(target);
        }
        let not_returned = match (truncated, defect) {
            (true, _) => MissingReason::Truncated,
            (false, Some(defect)) => MissingReason::Unparsable(defect),
            (false, None) => MissingReason::NotReturned,
        };
        for (id, _) in &self.expected {
            if !self.accepted.contains_key(id) && !seen.contains(id) {
                self.missing_reasons.insert(*id, not_returned.clone());
            }
        }
        report
    }

    fn accept(&mut self, entry: AiMtTranslation, report: &mut AbsorbReport) {
        let id = entry.bubble_id;
        self.missing_reasons.remove(&id);
        self.accepted.insert(id, entry);
        report.newly_accepted.push(id);
    }

    fn mark_missing(&mut self, reason: &MissingReason) {
        for (id, _) in &self.expected {
            if !self.accepted.contains_key(id) {
                self.missing_reasons.insert(*id, reason.clone());
            }
        }
    }

    /// How many elements of `value` this collector would accept (ignoring what is already
    /// accepted): the measure `absorb` picks among JSON candidates by.
    fn score(&self, value: &Value) -> usize {
        let single_target = match self.expected.as_slice() {
            [(target, kind)] if self.lenient_single => Some((*target, *kind)),
            _ => None,
        };
        elements_of(value).map_or(0, |elements| {
            elements
                .into_iter()
                .filter(|element| match element_id(element).and_then(|id| self.kind_of(id).map(|kind| (id, kind))) {
                    Some((id, kind)) => parse_entry(element, kind, id).is_ok(),
                    None => single_target.is_some_and(|(target, kind)| parse_entry(element, kind, target).is_ok()),
                })
                .count()
        })
    }

    fn kind_of(&self, id: i64) -> Option<ExpectedKind> {
        self.expected.iter().find(|(expected, _)| *expected == id).map(|(_, kind)| *kind)
    }

    /// The accepted translation of `id`.
    pub(crate) fn accepted(&self, id: i64) -> Option<&AiMtTranslation> {
        self.accepted.get(&id)
    }

    /// Expected ids without an accepted translation, in the request's order.
    pub(crate) fn missing_ids(&self) -> Vec<i64> {
        self.expected.iter().map(|(id, _)| *id).filter(|id| !self.accepted.contains_key(id)).collect()
    }

    /// The ONE repair retry for the answer `report` describes: `None` when every expected id
    /// is accepted. The message re-asks only the ids still missing or invalid; the complete
    /// elements of a cut-off answer were salvaged by `absorb`, so its retry asks only for what
    /// it did not reach, a smaller answer.
    pub(crate) fn repair_request(&self, report: &AbsorbReport) -> Option<RepairRequest> {
        let missing = self.missing_ids();
        if missing.is_empty() {
            return None;
        }
        let malformed = report.parse_error.is_some() || report.not_a_list || report.entries == 0;
        let reason = if report.truncated {
            RetryReason::Truncated
        } else if malformed {
            RetryReason::Malformed
        } else {
            RetryReason::Incomplete { missing: missing.len(), expected: self.expected.len() }
        };
        let mut lines = vec![match reason {
            RetryReason::Truncated => "Your previous answer was cut off by the output length limit before it was complete.".to_string(),
            RetryReason::Malformed => {
                let feedback = report.parse_error.as_ref().map_or_else(|| "It did not contain the list of items.".to_string(), JsonExtractError::model_feedback);
                format!("Your previous answer could not be used. {feedback}")
            }
            RetryReason::Incomplete { .. } | RetryReason::Empty | RetryReason::StructuredOutputUnsupported => "Your previous answer was incomplete or contained invalid entries:".to_string(),
        }];
        lines.extend(report.problems.iter().take(MAX_LISTED_PROBLEMS).map(|problem| format!("- {}", problem.model_text())));
        if report.problems.len() > MAX_LISTED_PROBLEMS {
            lines.push(format!("- … and {} more problems.", report.problems.len() - MAX_LISTED_PROBLEMS));
        }
        lines.push(self.reask_instruction(&missing));
        Some(RepairRequest { reason, message: lines.join("\n") })
    }

    /// The instruction part of the repair message, in the request's answer form.
    fn reask_instruction(&self, missing: &[i64]) -> String {
        const SHAPES: &str = "Text items: {\"id\": number, \"translation\": string}. Single-area image items: {\"id\": number, \"original_text\": string, \"translation\": string}. Multi-area image items: {\"id\": number, \"areas\": [{\"original_text\": string, \"translation\": string}, ...]} with exactly area_count entries in the input order.";
        let ids = missing.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        match self.form {
            AnswerForm::Array => format!("Answer again with ONLY a flat JSON array holding one entry for each of these ids: [{ids}]. Do not repeat ids that were already answered correctly and never include context items. {SHAPES} No markdown, no commentary."),
            AnswerForm::WrappedArray => format!("Answer again with ONLY a JSON object {{\"items\": [...]}} whose array holds one entry for each of these ids: [{ids}]. Do not repeat ids that were already answered correctly and never include context items. {SHAPES} No markdown, no commentary."),
            AnswerForm::Object => format!("Answer again with ONLY the single JSON object for id {ids} exactly as instructed: {{\"id\": number, \"original_text\": string, \"translation\": string}}, or for a multi-area bubble {{\"id\": number, \"areas\": [{{\"original_text\": string, \"translation\": string}}, ...]}} with exactly area_count entries. No markdown, no commentary."),
        }
    }

    /// The localized reason `id` has no accepted translation, for the run's warnings list.
    pub(crate) fn failure_text(&self, id: i64) -> String {
        let kind = self.kind_of(id).unwrap_or(ExpectedKind::Text);
        match self.missing_reasons.get(&id).unwrap_or(&MissingReason::NotReturned) {
            MissingReason::NotReturned => not_returned_text(kind),
            MissingReason::Truncated => t!("translation.mt.ai_answer_truncated_error").to_string(),
            MissingReason::Unparsable(JsonExtractError::NotFound) => t!("translation.mt.ai_no_json_error").to_string(),
            MissingReason::Unparsable(JsonExtractError::Unterminated) => t!("translation.mt.ai_json_cut_off_error").to_string(),
            MissingReason::Unparsable(JsonExtractError::Invalid { detail }) => tf!("translation.mt.invalid_json_error", err = detail),
            MissingReason::NotAList => t!("translation.mt.json_not_array_error").to_string(),
            MissingReason::Element(problem) => match problem {
                ElementProblem::MissingTranslation(_) => t!("translation.mt.ai_empty_translation_error").to_string(),
                ElementProblem::MissingOriginal(_) => t!("translation.mt.ai_no_original_translation_error").to_string(),
                ElementProblem::MissingAreas(_) => t!("translation.mt.ai_no_areas_error").to_string(),
                ElementProblem::AreaCount { expected, got, .. } => tf!("translation.mt.ai_area_count_error", got = got, expected = expected),
                ElementProblem::NotAnObject { .. } | ElementProblem::NoId { .. } | ElementProblem::UnknownId(_) | ElementProblem::DuplicateId(_) => not_returned_text(kind),
            },
        }
    }

    /// The canonical answer to keep in the batch chat history: every accepted translation in
    /// the request's order — a flat array, or `{"items": [...]}` for `AnswerForm::WrappedArray`
    /// — so later batches see a clean, compliant turn instead of the raw (possibly malformed)
    /// text and the repair turns. `AnswerForm::Object` (per-ImageBubble mode, which keeps no
    /// chat history) also yields the array form.
    pub(crate) fn history_json(&self) -> String {
        let entries: Vec<Value> = self.expected.iter().filter_map(|(id, kind)| self.accepted.get(id).map(|entry| entry_json(entry, *kind))).collect();
        let value = match self.form {
            AnswerForm::Array | AnswerForm::Object => Value::Array(entries),
            AnswerForm::WrappedArray => json!({ "items": entries }),
        };
        value.to_string()
    }
}

/// Localized "not returned" reason, by item kind (the pre-existing per-kind texts).
fn not_returned_text(kind: ExpectedKind) -> String {
    match kind {
        ExpectedKind::Text => t!("translation.mt.ai_no_translation_for_id_error").to_string(),
        ExpectedKind::SingleImage => t!("translation.mt.ai_no_original_translation_error").to_string(),
        ExpectedKind::MultiImage { .. } => t!("translation.mt.ai_no_areas_error").to_string(),
    }
}

/// The item list of an answer: an array, an object wrapping one under `items` /
/// `translations`, or a single item object (`id` / `bubble_id` / `areas` / `translation`).
fn elements_of(value: &Value) -> Option<Vec<&Value>> {
    if let Some(array) = value.as_array() {
        return Some(array.iter().collect());
    }
    let object = value.as_object()?;
    if let Some(array) = object.get("items").or_else(|| object.get("translations")).and_then(Value::as_array) {
        return Some(array.iter().collect());
    }
    ["id", "bubble_id", "areas", "translation"].iter().any(|key| object.contains_key(*key)).then(|| vec![value])
}

/// The element's id: `id` or `bubble_id`, a JSON integer or a numeric string.
fn element_id(element: &Value) -> Option<i64> {
    let raw = element.get("id").or_else(|| element.get("bubble_id"))?;
    raw.as_i64().or_else(|| raw.as_str().and_then(|text| text.trim().parse::<i64>().ok()))
}

/// The trimmed string of the first present key of `keys`.
fn trimmed_str<'a>(element: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| element.get(*key)).and_then(Value::as_str).map(str::trim)
}

/// Validates one element for an item of `kind` and builds its translation for `id`.
fn parse_entry(element: &Value, kind: ExpectedKind, id: i64) -> Result<AiMtTranslation, ElementProblem> {
    let translation = trimmed_str(element, &["translation", "text"]).filter(|text| !text.is_empty()).map(str::to_string);
    let original_text = trimmed_str(element, &["original_text", "original"]).filter(|text| !text.is_empty()).map(str::to_string);
    match kind {
        ExpectedKind::Text => {
            let translation = translation.ok_or(ElementProblem::MissingTranslation(id))?;
            Ok(AiMtTranslation { bubble_id: id, original_text, translation, areas: Vec::new() })
        }
        ExpectedKind::SingleImage => {
            let translation = translation.ok_or(ElementProblem::MissingTranslation(id))?;
            let original_text = original_text.ok_or(ElementProblem::MissingOriginal(id))?;
            Ok(AiMtTranslation { bubble_id: id, original_text: Some(original_text), translation, areas: Vec::new() })
        }
        ExpectedKind::MultiImage { area_count } => {
            let raw_areas = element.get("areas").and_then(Value::as_array).ok_or(ElementProblem::MissingAreas(id))?;
            let areas: Vec<AiMtArea> = raw_areas.iter().filter_map(parse_area).collect();
            if areas.len() != area_count || raw_areas.len() != area_count {
                return Err(ElementProblem::AreaCount { id, expected: area_count, got: areas.len() });
            }
            if areas.iter().all(|area| area.translation.is_empty()) {
                return Err(ElementProblem::MissingTranslation(id));
            }
            Ok(AiMtTranslation { bubble_id: id, original_text: None, translation: String::new(), areas })
        }
    }
}

/// One area `{original_text, translation}`; `None` when `translation` is not a string (an empty
/// string is a valid area without text).
fn parse_area(value: &Value) -> Option<AiMtArea> {
    let translation = trimmed_str(value, &["translation", "text"])?.to_string();
    let original_text = trimmed_str(value, &["original_text", "original"]).unwrap_or_default().to_string();
    Some(AiMtArea { original_text, translation })
}

/// The canonical JSON of an accepted translation (the shape its kind asks for).
fn entry_json(entry: &AiMtTranslation, kind: ExpectedKind) -> Value {
    match kind {
        ExpectedKind::Text => json!({ "id": entry.bubble_id, "translation": entry.translation }),
        ExpectedKind::SingleImage => json!({ "id": entry.bubble_id, "original_text": entry.original_text.clone().unwrap_or_default(), "translation": entry.translation }),
        ExpectedKind::MultiImage { .. } => {
            let areas: Vec<Value> = entry.areas.iter().map(|area| json!({ "original_text": area.original_text, "translation": area.translation })).collect();
            json!({ "id": entry.bubble_id, "areas": areas })
        }
    }
}

/// The JSON schema of a whole answer for provider structured output: in `AnswerForm::Object`
/// the item schema of `kinds[0]`; otherwise `{"items": [item]}` (strict structured output needs
/// an object at the top level), the item schema being an `anyOf` of the distinct kinds present.
/// Every property is required, as strict mode demands.
pub(crate) fn strict_answer_schema(kinds: &[ExpectedKind], form: AnswerForm) -> Value {
    let mut distinct: Vec<Value> = Vec::new();
    for kind in kinds {
        let schema = item_schema(*kind);
        if !distinct.contains(&schema) {
            distinct.push(schema);
        }
    }
    let item = match distinct.len() {
        0 => item_schema(ExpectedKind::Text),
        1 => distinct.remove(0),
        _ => json!({ "anyOf": distinct }),
    };
    match form {
        AnswerForm::Object => item,
        AnswerForm::Array | AnswerForm::WrappedArray => strict_object_schema(&[("items", json!({ "type": "array", "items": item }))]),
    }
}

/// The strict schema of one item of `kind`.
fn item_schema(kind: ExpectedKind) -> Value {
    let string = json!({ "type": "string" });
    let id = ("id", json!({ "type": "integer" }));
    match kind {
        ExpectedKind::Text => strict_object_schema(&[id, ("translation", string)]),
        ExpectedKind::SingleImage => strict_object_schema(&[id, ("original_text", string.clone()), ("translation", string)]),
        ExpectedKind::MultiImage { .. } => {
            let area = strict_object_schema(&[("original_text", string.clone()), ("translation", string)]);
            strict_object_schema(&[id, ("areas", json!({ "type": "array", "items": area }))])
        }
    }
}

/// A doubt about an accepted translation. Marks the item only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentWarning {
    /// The translation equals the source text (ignoring case, spaces and punctuation).
    SameAsSource,
    /// The translation is still written in the source's writing system, which the target
    /// language does not use.
    SourceScript,
    /// The translation is more than `TOO_LONG_RATIO` times longer than its (weighted) source.
    TooLong { source_chars: usize, translation_chars: usize },
}

impl ContentWarning {
    /// Localized text for the run's warnings list.
    pub(crate) fn text(self) -> String {
        match self {
            Self::SameAsSource => t!("translation.mt.warning_same_as_source").to_string(),
            Self::SourceScript => t!("translation.mt.warning_source_script").to_string(),
            Self::TooLong { source_chars, translation_chars } => tf!("translation.mt.warning_too_long", translation = translation_chars, source = source_chars),
        }
    }

    fn same_variant(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// The content checks of `entry` (an item of `kind` whose source text is `source_text`) for
/// translation into `target_lang` (an MT language code such as `ru`, `en`, `zh-CN`). Each kind
/// of warning is reported once per item. Image items are checked against the text the model
/// read (`original_text`, per area), falling back to `source_text`.
pub(crate) fn content_warnings(kind: ExpectedKind, source_text: &str, entry: &AiMtTranslation, target_lang: &str) -> Vec<ContentWarning> {
    let pairs: Vec<(&str, &str)> = match kind {
        ExpectedKind::Text => vec![(source_text, entry.translation.as_str())],
        ExpectedKind::SingleImage => vec![(entry.original_text.as_deref().unwrap_or(source_text), entry.translation.as_str())],
        ExpectedKind::MultiImage { .. } => entry.areas.iter().map(|area| (area.original_text.as_str(), area.translation.as_str())).collect(),
    };
    let target_scripts = target_scripts(target_lang);
    let mut warnings: Vec<ContentWarning> = Vec::new();
    for (source, translation) in pairs {
        for warning in pair_warnings(source, translation, target_scripts) {
            if !warnings.iter().any(|known| known.same_variant(warning)) {
                warnings.push(warning);
            }
        }
    }
    warnings
}

/// The checks of one source / translation pair.
fn pair_warnings(source: &str, translation: &str, target_scripts: Option<&[Script]>) -> Vec<ContentWarning> {
    let mut warnings = Vec::new();
    let source = source.trim();
    let translation = translation.trim();
    if source.is_empty() || translation.is_empty() {
        return warnings;
    }
    let normalized_source = normalize_for_compare(source);
    if normalized_source.chars().filter(|c| c.is_alphabetic()).count() >= SAME_AS_SOURCE_MIN_LETTERS && normalized_source == normalize_for_compare(translation) {
        warnings.push(ContentWarning::SameAsSource);
    }
    if let (Some(targets), Some(source_script), Some(translation_script)) = (target_scripts, dominant_script(source), dominant_script(translation))
        && !targets.contains(&translation_script)
        && (translation_script == source_script || (translation_script.is_japanese() && source_script.is_japanese()))
    {
        warnings.push(ContentWarning::SourceScript);
    }
    let source_chars = source.chars().count();
    let source_weight: usize = source.chars().map(|c| if Script::of(c).is_some_and(Script::is_cjk) { CJK_SOURCE_CHAR_WEIGHT } else { 1 }).sum();
    let translation_chars = translation.chars().count();
    if translation_chars > TOO_LONG_MIN_CHARS && translation_chars > source_weight.saturating_mul(TOO_LONG_RATIO) {
        warnings.push(ContentWarning::TooLong { source_chars, translation_chars });
    }
    warnings
}

/// Lowercased letters and digits only.
fn normalize_for_compare(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A writing system, as far as the content checks need to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Latin,
    Cyrillic,
    Hangul,
    Kana,
    Han,
}

impl Script {
    /// Kana and Han both write Japanese.
    fn is_japanese(self) -> bool {
        matches!(self, Self::Kana | Self::Han)
    }

    /// Hangul, kana and Han: dense scripts where one char is about a word.
    fn is_cjk(self) -> bool {
        matches!(self, Self::Hangul | Self::Kana | Self::Han)
    }

    fn of(c: char) -> Option<Self> {
        match u32::from(c) {
            0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F | 0x1E00..=0x1EFF => Some(Self::Latin),
            0x400..=0x52F => Some(Self::Cyrillic),
            0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => Some(Self::Hangul),
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9D => Some(Self::Kana),
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => Some(Self::Han),
            _ => None,
        }
    }
}

/// The script of at least 60 % of the classified letters of `text` (and at least 3 of them);
/// `None` when unsure.
fn dominant_script(text: &str) -> Option<Script> {
    const SCRIPTS: [Script; 5] = [Script::Latin, Script::Cyrillic, Script::Hangul, Script::Kana, Script::Han];
    let mut counts = [0_usize; SCRIPTS.len()];
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        if let Some(script) = Script::of(c)
            && let Some(slot) = SCRIPTS.iter().position(|known| *known == script)
        {
            counts[slot] += 1;
        }
    }
    let total: usize = counts.iter().sum();
    let (slot, best) = counts.iter().enumerate().max_by_key(|(_, count)| **count)?;
    (total >= 3 && best.saturating_mul(10) >= total.saturating_mul(6)).then_some(SCRIPTS[slot])
}

/// The scripts a target language is written in; `None` for a language this check does not know
/// (no `SourceScript` warning is raised then).
fn target_scripts(target_lang: &str) -> Option<&'static [Script]> {
    let code = target_lang.trim().split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    match code.as_str() {
        "ru" | "uk" | "be" | "bg" | "sr" | "mk" | "kk" | "ky" | "mn" | "tg" => Some(&[Script::Cyrillic]),
        "ko" => Some(&[Script::Hangul]),
        "ja" => Some(&[Script::Kana, Script::Han]),
        "zh" => Some(&[Script::Han]),
        "en" | "es" | "fr" | "de" | "pt" | "it" | "pl" | "nl" | "tr" | "id" | "vi" | "cs" | "ro" | "hu" | "sv" | "da" | "fi" | "no" | "nb" | "ms" | "tl" | "hr" | "sk" | "sl" | "lt" | "lv" | "et" | "ca" | "az" | "uz" => Some(&[Script::Latin]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AnswerForm, BatchCollector, ContentWarning, ElementProblem, ExpectedKind, MissingReason, content_warnings, strict_answer_schema};
    use crate::machine_translation::{MtImageArea, MtImageInput, MtImageSource, MtTranslateItem};
    use ms_ai_api::RetryReason;

    fn item(bubble_id: i64, needs_translation: bool) -> MtTranslateItem {
        MtTranslateItem { bubble_id, page_idx: 0, img_v: 0.0, order: 0, character: String::new(), text: format!("text {bubble_id}"), existing_translation: String::new(), image: None, needs_translation }
    }

    fn image_item(bubble_id: i64, areas: usize) -> MtTranslateItem {
        let area = MtImageArea { description: String::new(), original: String::new(), rel_bbox: None };
        MtTranslateItem { image: Some(MtImageInput { description: String::new(), source: MtImageSource::ExternalPath(format!("image_bubbles/{bubble_id}.png")), areas: vec![area; areas] }), text: String::new(), ..item(bubble_id, true) }
    }

    fn collector(ids: &[i64]) -> BatchCollector {
        let items: Vec<MtTranslateItem> = ids.iter().map(|id| item(*id, true)).collect();
        BatchCollector::new(&items, AnswerForm::Array, false)
    }

    #[test]
    fn a_clean_answer_accepts_every_id() {
        let mut batch = collector(&[1, 2]);
        let report = batch.absorb(r#"[{"id":1,"translation":"один"},{"id":2,"translation":"два"}]"#, false);
        assert_eq!(report.newly_accepted, vec![1, 2]);
        assert!(batch.missing_ids().is_empty());
        assert_eq!(batch.repair_request(&report), None);
        assert_eq!(batch.accepted(2).map(|entry| entry.translation.as_str()), Some("два"));
    }

    #[test]
    fn prose_think_and_fences_around_the_answer_are_tolerated() {
        let mut batch = collector(&[1]);
        let report = batch.absorb("<think>let me see</think>Sure!\n```json\n[{\"id\": \"1\", \"translation\": \"привет\",}]\n```", false);
        assert_eq!(report.newly_accepted, vec![1]);
    }

    #[test]
    fn one_bad_element_does_not_fail_the_batch() {
        let mut batch = collector(&[1, 2, 3]);
        let report = batch.absorb(r#"[{"id":1,"translation":"a"},{"id":2,"translation":""},{"translation":"orphan"},{"id":99,"translation":"x"},{"id":1,"translation":"dup"}]"#, false);
        assert_eq!(report.newly_accepted, vec![1]);
        assert_eq!(report.problems, vec![ElementProblem::MissingTranslation(2), ElementProblem::NoId { position: 2 }, ElementProblem::UnknownId(99), ElementProblem::DuplicateId(1)]);
        assert_eq!(batch.missing_ids(), vec![2, 3]);
        assert_eq!(batch.accepted(1).map(|entry| entry.translation.as_str()), Some("a"), "the first copy of a duplicate wins");
        let repair = batch.repair_request(&report).expect("missing ids are re-asked");
        assert_eq!(repair.reason, RetryReason::Incomplete { missing: 2, expected: 3 });
        assert!(repair.message.contains("[2, 3]"), "only the missing ids are re-asked: {}", repair.message);
        assert!(repair.message.contains("id 99 was not requested"));
    }

    #[test]
    fn the_retry_merges_without_overwriting_accepted_ids() {
        let mut batch = collector(&[1, 2]);
        batch.absorb(r#"[{"id":1,"translation":"first"}]"#, false);
        let report = batch.absorb(r#"[{"id":1,"translation":"changed"},{"id":2,"translation":"second"}]"#, false);
        assert_eq!(report.newly_accepted, vec![2]);
        assert_eq!(batch.accepted(1).map(|entry| entry.translation.as_str()), Some("first"));
        assert_eq!(batch.history_json(), json!([{"id":1,"translation":"first"},{"id":2,"translation":"second"}]).to_string());
    }

    #[test]
    fn context_ids_are_ignored_silently() {
        let items = vec![item(1, false), item(2, true)];
        let mut batch = BatchCollector::new(&items, AnswerForm::Array, false);
        let report = batch.absorb(r#"[{"id":1,"translation":"ctx"},{"id":2,"translation":"ok"}]"#, false);
        assert!(report.problems.is_empty());
        assert_eq!(report.newly_accepted, vec![2]);
    }

    #[test]
    fn an_unparsable_answer_asks_for_everything_again() {
        let mut batch = collector(&[4, 5]);
        let report = batch.absorb("I'm sorry, I can't help with that.", false);
        let repair = batch.repair_request(&report).expect("retry");
        assert_eq!(repair.reason, RetryReason::Malformed);
        assert!(repair.message.contains("no JSON") && repair.message.contains("[4, 5]"), "{}", repair.message);
    }

    #[test]
    fn a_truncated_answer_applies_its_complete_elements_and_re_asks_the_rest() {
        let mut batch = collector(&[1, 2, 3]);
        let report = batch.absorb(r#"[{"id":1,"translation":"a"},{"id":2,"translation":"b"},{"id":3,"transl"#, true);
        assert_eq!(report.newly_accepted, vec![1, 2]);
        let repair = batch.repair_request(&report).expect("retry");
        assert_eq!(repair.reason, RetryReason::Truncated);
        assert!(repair.message.contains("[3]") && !repair.message.contains("[1, 2, 3]"), "{}", repair.message);
        assert_eq!(batch.missing_reasons.get(&3), Some(&MissingReason::Truncated));
        let mut closed = collector(&[1, 2, 3]);
        let report = closed.absorb(r#"[{"id":1,"translation":"a"}]"#, true);
        assert_eq!(report.newly_accepted, vec![1]);
        assert!(closed.repair_request(&report).expect("retry").message.contains("[2, 3]"));
    }

    #[test]
    fn a_cut_off_strict_wrapper_is_salvaged_too() {
        let items = vec![item(1, true), item(2, true)];
        let mut batch = BatchCollector::new(&items, AnswerForm::WrappedArray, false);
        let report = batch.absorb(r#"{"items":[{"id":1,"translation":"a"},{"id":2,"tra"#, true);
        assert_eq!(report.newly_accepted, vec![1]);
        assert_eq!(batch.missing_ids(), vec![2]);
    }

    #[test]
    fn a_broken_element_keeps_the_valid_ones_and_reports_the_parse_error() {
        let mut batch = collector(&[1, 2, 3]);
        let report = batch.absorb(r#"[{"id":1,"translation":"a"},{"id":2,"translation":'b'},{"id":3,"translation":"c"}]"#, false);
        assert_eq!(report.newly_accepted, vec![1, 3]);
        let repair = batch.repair_request(&report).expect("retry");
        assert_eq!(repair.reason, RetryReason::Malformed);
        assert!(repair.message.contains("could not be parsed") && repair.message.contains("[2]"), "{}", repair.message);
    }

    #[test]
    fn the_candidate_answering_the_most_ids_wins() {
        let mut batch = collector(&[1, 2]);
        let report = batch.absorb("Translated [2] items:\n[{\"id\":1,\"translation\":\"a\"},{\"id\":2,\"translation\":\"b\"}]", false);
        assert_eq!(report.newly_accepted, vec![1, 2]);
        let mut drafted = collector(&[1]);
        drafted.absorb("Draft: [{\"id\":1,\"translation\":\"X\"}]\nFinal: [{\"id\":1,\"translation\":\"Y\"}]", false);
        assert_eq!(drafted.accepted(1).map(|entry| entry.translation.as_str()), Some("Y"));
    }

    #[test]
    fn think_tags_inside_a_translation_are_kept() {
        let mut batch = collector(&[1]);
        batch.absorb(r#"<think>plan</think>[{"id":1,"translation":"<think>a</think> b"}]"#, false);
        assert_eq!(batch.accepted(1).map(|entry| entry.translation.as_str()), Some("<think>a</think> b"));
    }

    #[test]
    fn the_strict_wrapper_and_the_translations_wrapper_are_read() {
        let items = vec![item(1, true)];
        let mut wrapped = BatchCollector::new(&items, AnswerForm::WrappedArray, false);
        assert_eq!(wrapped.absorb(r#"{"items":[{"id":1,"translation":"a"}]}"#, false).newly_accepted, vec![1]);
        assert_eq!(wrapped.history_json(), json!({"items":[{"id":1,"translation":"a"}]}).to_string());
        let mut legacy = BatchCollector::new(&items, AnswerForm::Array, false);
        assert_eq!(legacy.absorb(r#"{"translations":[{"id":1,"translation":"a"}]}"#, false).newly_accepted, vec![1]);
    }

    #[test]
    fn image_items_need_their_original_and_the_right_area_count() {
        let items = vec![image_item(7, 1), image_item(8, 2)];
        let mut batch = BatchCollector::new(&items, AnswerForm::Array, false);
        let report = batch.absorb(r#"[{"id":7,"translation":"БУМ"},{"id":8,"areas":[{"original_text":"A","translation":"А"}]}]"#, false);
        assert_eq!(report.problems, vec![ElementProblem::MissingOriginal(7), ElementProblem::AreaCount { id: 8, expected: 2, got: 1 }]);
        let report = batch.absorb(r#"[{"id":7,"original_text":"BOOM","translation":"БУМ"},{"id":8,"areas":[{"original_text":"A","translation":"А"},{"original_text":"B","translation":"Б"}]}]"#, false);
        assert_eq!(report.newly_accepted, vec![7, 8]);
        assert_eq!(batch.accepted(8).map(|entry| entry.areas.len()), Some(2));
    }

    #[test]
    fn the_single_image_target_tolerates_a_wrong_id() {
        let items = vec![image_item(10, 1)];
        let mut batch = BatchCollector::new(&items, AnswerForm::Object, true);
        let report = batch.absorb(r#"{"id": 3, "original_text": "BOOM", "translation": "БУМ"}"#, false);
        assert_eq!(report.newly_accepted, vec![10]);
        assert_eq!(batch.accepted(10).map(|entry| entry.bubble_id), Some(10));
    }

    /// The reasons behind `failure_text` (compared as data: the localized texts depend on the
    /// process-global catalog other tests may swap).
    #[test]
    fn missing_reasons_follow_the_answer() {
        let mut batch = collector(&[1, 2]);
        batch.absorb(r#"[{"id":1,"translation":""}]"#, false);
        assert_eq!(batch.missing_reasons.get(&1), Some(&MissingReason::Element(ElementProblem::MissingTranslation(1))));
        assert_eq!(batch.missing_reasons.get(&2), Some(&MissingReason::NotReturned));
        batch.absorb("[{\"id\":", true);
        assert_eq!(batch.missing_reasons.get(&2), Some(&MissingReason::Truncated));
        assert!(!batch.failure_text(2).is_empty());
        batch.absorb(r#"[{"id":1,"translation":"a"},{"id":2,"translation":"b"}]"#, false);
        assert!(batch.missing_reasons.is_empty(), "accepted ids have no reason left");
    }

    #[test]
    fn a_single_object_answer_is_one_item() {
        let items = vec![image_item(10, 1)];
        let mut batch = BatchCollector::new(&items, AnswerForm::Object, true);
        let report = batch.absorb(r#"{"id":10,"original_text":"BOOM","translation":"БУМ"}"#, false);
        assert_eq!(report.newly_accepted, vec![10]);
        assert_eq!(batch.accepted(10).and_then(|entry| entry.original_text.as_deref()), Some("BOOM"));
    }

    #[test]
    fn strict_schema_wraps_the_array_and_unites_kinds() {
        let schema = strict_answer_schema(&[ExpectedKind::Text, ExpectedKind::Text], AnswerForm::WrappedArray);
        assert_eq!(schema["required"], json!(["items"]));
        assert_eq!(schema["properties"]["items"]["items"]["required"], json!(["id", "translation"]));
        let mixed = strict_answer_schema(&[ExpectedKind::Text, ExpectedKind::MultiImage { area_count: 2 }], AnswerForm::WrappedArray);
        assert_eq!(mixed["properties"]["items"]["items"]["anyOf"].as_array().map(Vec::len), Some(2));
        let single = strict_answer_schema(&[ExpectedKind::SingleImage], AnswerForm::Object);
        assert_eq!(single["required"], json!(["id", "original_text", "translation"]));
    }

    fn text_entry(translation: &str) -> super::AiMtTranslation {
        super::AiMtTranslation { bubble_id: 1, original_text: None, translation: translation.to_string(), areas: Vec::new() }
    }

    #[test]
    fn content_checks_mark_doubtful_translations() {
        assert_eq!(content_warnings(ExpectedKind::Text, "WHERE ARE YOU GOING?", &text_entry("Where are you going"), "ru"), vec![ContentWarning::SameAsSource, ContentWarning::SourceScript]);
        assert_eq!(content_warnings(ExpectedKind::Text, "어디 가?", &text_entry("어디 가니?"), "ru"), vec![ContentWarning::SourceScript]);
        assert_eq!(content_warnings(ExpectedKind::Text, "どこへ行くの？", &text_entry("何処へ行く"), "en"), vec![ContentWarning::SourceScript]);
        let long = "Это очень длинный перевод, который явно содержит пояснения модели вместо перевода.";
        assert_eq!(content_warnings(ExpectedKind::Text, "Hi there", &text_entry(long), "ru"), vec![ContentWarning::TooLong { source_chars: 8, translation_chars: long.chars().count() }]);
    }

    #[test]
    fn content_checks_stay_quiet_when_unsure_or_fine() {
        assert!(content_warnings(ExpectedKind::Text, "어디 가?", &text_entry("Куда идёшь?"), "ru").is_empty());
        assert!(content_warnings(ExpectedKind::Text, "OK", &text_entry("OK"), "ru").is_empty(), "short texts are not compared");
        assert!(content_warnings(ExpectedKind::Text, "Hello there", &text_entry("Hello there"), "xx").contains(&ContentWarning::SameAsSource));
        assert!(!content_warnings(ExpectedKind::Text, "Hello there", &text_entry("Hola amigo"), "es").contains(&ContentWarning::SourceScript), "same script as the target is fine");
        assert!(content_warnings(ExpectedKind::Text, "Hello", &text_entry("Hola"), "xx").is_empty(), "unknown target language: no script check");
    }

    #[test]
    fn too_long_weighs_dense_cjk_sources() {
        let normal = "We have to get out of here right now, okay?";
        assert!(content_warnings(ExpectedKind::Text, "我们必须马上离开这里吧", &text_entry(normal), "en").is_empty(), "a normal CJK -> Latin translation is not marked");
        let rambling = "Ah! (This is an exclamation of surprise; the character is startled by the noise.)";
        assert!(matches!(content_warnings(ExpectedKind::Text, "啊!", &text_entry(rambling), "en").as_slice(), [ContentWarning::TooLong { .. }]));
    }
}
