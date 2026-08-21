/*
File: src/widgets/autocomplete_line.rs

Purpose:
Reusable stateful single-line input with SEGMENT-based autocompletion: a speculative
inline completion drawn straight into the caller's value plus a popup list of ranked
suggestions.

Main responsibilities:
- cut the typed line into QUERY SEGMENTS — every suffix of the text before the caret that
  starts at a word/punctuation boundary — instead of matching the whole line, so candidates
  that contain spaces ("Кон Че Мин") still complete in the middle of a sentence;
- collect variants from ALL of those segments and rank them from the segment NEAREST THE
  CARET outwards, inside each segment prefix matches before word-boundary matches whose
  insertion is the candidate's TAIL ("Су" -> "Су Лин");
- keep long descriptive candidates out of mid-line completion, drop variants that would add
  nothing, and collapse variants that would splice to the same line;
- splice the chosen insertion over the segment only, keeping the rest of the line and
  putting the caret right after the insertion;
- refuse to WRITE a speculative completion until the guess is strong enough (see the gates
  below), while still listing options in the popup;
- keep a snapshot of what the user actually typed so `Escape` can roll a speculative
  completion back to it.

Key structures:
- `AutocompleteLine`: widget state (id, suggestion limit, highlight, typed snapshot,
  pending speculative completion).
- `AutocompleteLineResponse`: per-frame result (changed / submitted / applied / popup).
- `Suggestion`: a candidate, the byte offset inside it where its insertion starts, and the
  byte offset in the TYPED text of the segment it replaces.
- `SuggestionSet`: bounded accumulator that drops useless variants and deduplicates by the
  LINE a variant would produce.
- `SuggestionList`: the frame's ranked variants plus the shared caret offset.
- `CaretWriter`: the three values needed to write a caret back into `egui`'s text state.

Key functions:
- `AutocompleteLine::draw`: the whole frame (text edit, inline completion, hotkeys, popup).
- `resolve_suggestions`: pure cross-segment collection + ranking.
- `is_useless_variant`: the pure "this offer adds nothing" filter.
- `inline_insertion_allowed`: the pure gate on writing into the caller's value.
- `splice_segment`: pure "replace the segment with the insertion" text edit.

Notes:
The widget mutates `value` SPECULATIVELY while drawing the inline completion. The exact
speculative value is remembered in `AutocompleteLine::completion`, so "is a completion
standing?" is answered by comparison with that snapshot and never guessed from the text —
a segment completion changes the middle of the line, where a prefix heuristic fails.
Writing into `value` is deliberately conservative, because the candidate's spelling wins
over the user's and a wrong guess is felt as text that "cannot be fixed": it needs at least
`MIN_INLINE_SIGNIFICANT_CHARS` typed chars in the segment and exactly one surviving variant.
No segment "wins" the frame: the caller saves unfinished input back into its own candidate
list, so one stale entry matching the whole line used to take the win and hide the real
completion of the two letters at the caret. Long descriptive candidates
(`MAX_COMPLETION_CANDIDATE_WORDS`) are dropped from matching per segment, before anything is
collected, so they cannot crowd the ranked list either.
All case-insensitive matching streams `char::to_lowercase` instead of allocating lowercase
copies, because a byte offset found in a lowercased copy cannot be mapped back onto the
original string (`to_lowercase` may change both char count and byte length).
*/
// The builder-style helpers (`with_max_suggestions`, `with_hint_text`) mirror the setters
// that the single in-tree caller uses and complete the widget's public surface; they have
// no call site yet. Keeping the pair symmetric is worth one crate-local allow.
#![allow(dead_code)]

use eframe::egui;
use egui::text_edit::TextEditState;
use egui::{Id, Key};

const DEFAULT_MAX_SUGGESTIONS: usize = 8;

/// Punctuation that ends an autocomplete segment, in addition to any whitespace.
///
/// `-` and `'` are deliberately absent: both occur inside personal names, which are the
/// candidates this widget completes.
const SEGMENT_SEPARATORS: [char; 14] = [
    ',', ';', ':', '/', '|', '(', ')', '[', ']', '{', '}', '"', '«', '»',
];

/// Horizontal gap, in points, between the inserted text and the dimmed full candidate in a
/// popup entry. A gap rather than a worded separator, so nothing here needs localization.
const SUGGESTION_HINT_GAP: f32 = 12.0;

/// Largest number of whitespace-separated parts a candidate may have and still take part in
/// completion everywhere.
///
/// Four is measured against this data set: the longest transliterated personal name here is
/// three parts ("Кон Че Мин"), and four leaves room for one title or suffix. Beyond that a
/// "candidate" is a descriptive row label — "Разговор Ын Соён и Пак Соэ по телефону" — and
/// splicing it, or a tail of it, into the middle of a sentence is never what the user meant.
const MAX_COMPLETION_CANDIDATE_WORDS: usize = 4;

/// Fewest non-whitespace chars a segment must hold before the widget may WRITE a speculative
/// completion into the caller's value.
///
/// One char is far too weak a signal: a lone conjunction ("и") matched a character name,
/// and because the candidate's spelling wins the user's lowercase "и" was silently
/// capitalised in text they had already typed. The popup is NOT gated by this — offering
/// options costs the user nothing, overwriting their text does.
const MIN_INLINE_SIGNIFICANT_CHARS: usize = 2;

/// Result of one `AutocompleteLine::draw` call.
///
/// `selected_suggestion` always carries the FULL candidate, even when only its tail was
/// inserted into the text, so callers can key off the canonical name.
#[derive(Debug, Clone, Default)]
pub struct AutocompleteLineResponse {
    pub changed: bool,
    pub submitted: bool,
    pub suggestion_applied: bool,
    pub popup_open: bool,
    pub selected_suggestion: Option<String>,
}

/// A speculative inline completion the widget wrote into the caller's value.
#[derive(Debug, Clone)]
struct PendingCompletion {
    /// Exactly what the widget stored. The completion counts as standing only while the
    /// caller's value still equals this.
    value: String,
    /// Char offset just past the insertion — where the caret goes when it is accepted.
    caret_char: usize,
    /// Byte offset just past the insertion — the typed-snapshot caret after accepting.
    caret_byte: usize,
    /// The full candidate, reported as `AutocompleteLineResponse::selected_suggestion`.
    full: String,
}

/// Single-line text input with segment autocompletion.
///
/// State is UI-only: the caller owns the text. The widget additionally remembers what the
/// user typed before any speculative completion, so `Escape` can restore it.
#[derive(Debug)]
pub struct AutocompleteLine {
    id: Id,
    max_suggestions: usize,
    hint_text: String,
    highlighted_idx: Option<usize>,
    keep_popup_open: bool,
    /// The line as the USER typed it, without any speculative completion.
    typed_text: String,
    /// Byte offset of the caret inside `typed_text`; the query segment ends here.
    typed_caret: usize,
    /// Set while a speculative inline completion stands in the caller's value.
    completion: Option<PendingCompletion>,
    /// Last value observed, used to drop the popup highlight when the text changes.
    last_value: String,
}

impl AutocompleteLine {
    /// Creates a widget whose egui ids derive from `id_source`.
    ///
    /// `id_source` must be stable across frames and unique per field; a localized label is
    /// not a valid source (it changes with the UI language).
    pub fn new(id_source: impl std::hash::Hash + std::fmt::Debug) -> Self {
        Self {
            id: Id::new(id_source),
            max_suggestions: DEFAULT_MAX_SUGGESTIONS,
            hint_text: String::new(),
            highlighted_idx: None,
            keep_popup_open: false,
            typed_text: String::new(),
            typed_caret: 0,
            completion: None,
            last_value: String::new(),
        }
    }

    /// Sets how many suggestions the popup may list. Values below 1 are clamped to 1.
    pub fn set_max_suggestions(&mut self, max_suggestions: usize) {
        self.max_suggestions = max_suggestions.max(1);
    }

    /// Builder form of [`Self::set_max_suggestions`].
    #[must_use]
    pub fn with_max_suggestions(mut self, max_suggestions: usize) -> Self {
        self.set_max_suggestions(max_suggestions);
        self
    }

    /// Sets the placeholder shown while the field is empty. Callers pass localized text.
    pub fn set_hint_text(&mut self, hint_text: impl Into<String>) {
        self.hint_text = hint_text.into();
    }

    /// Builder form of [`Self::set_hint_text`].
    #[must_use]
    pub fn with_hint_text(mut self, hint_text: impl Into<String>) -> Self {
        self.set_hint_text(hint_text);
        self
    }

    /// Draws the field for one frame and reports what happened.
    ///
    /// `value` is the caller's text and MAY be mutated speculatively: while the user types,
    /// the widget writes the best completion into it and selects the added tail, so the
    /// next keystroke overwrites it. `Escape` restores the typed text; `Tab`/`→` accept the
    /// inline completion; `↑`/`↓` walk the popup; `Enter` applies the highlighted entry or
    /// submits. `options` is the candidate list; only the first `max_suggestions` matches
    /// are shown.
    ///
    /// Performs no I/O and no allocation proportional to `options` beyond the bounded
    /// suggestion list.
    pub fn draw<S: AsRef<str>>(
        &mut self,
        ui: &mut egui::Ui,
        value: &mut String,
        options: &[S],
    ) -> AutocompleteLineResponse {
        let mut out = AutocompleteLineResponse::default();
        let text_id = self.id.with("text");
        let popup_id = self.id.with("popup");
        // Cloning the `Context` handle (an `Arc`) releases the shared borrow of `ui`, so the
        // caret writer built below can live across the rest of the frame.
        let ctx = ui.ctx().clone();
        let value_before_edit = value.clone();

        let mut text_edit = egui::TextEdit::singleline(value).id(text_id);
        if !self.hint_text.is_empty() {
            text_edit = text_edit.hint_text(self.hint_text.as_str());
        }
        let mut text_output = text_edit.show(ui);
        let (has_focus, text_changed, lost_focus, field_rect) = {
            // egui 0.35: `TextEditOutput::response` is an `AtomLayoutResponse`; the inner
            // `Response` is what the rest of this frame reads (rect field access needs it).
            let text_response = &text_output.response.response;
            (
                text_response.has_focus(),
                text_response.changed(),
                text_response.lost_focus(),
                text_response.rect,
            )
        };

        if *value != self.last_value {
            self.highlighted_idx = None;
            self.last_value.clone_from(value);
        }

        let cursor_range = text_output.state.cursor.char_range();
        let caret_is_collapsed = cursor_range.is_some_and(|range| range.is_empty());
        let caret_byte = cursor_range.map_or(value.len(), |range| {
            byte_offset_of_char(value, range.primary.index.0)
        });
        let deletion_like_change =
            text_changed && is_deletion_like_change(ui, &value_before_edit, value);
        let completion_standing = self
            .completion
            .as_ref()
            .is_some_and(|completion| completion.value == *value);

        if text_changed {
            // The user typed: this keystroke defines the new authoritative snapshot.
            self.typed_text.clone_from(value);
            self.typed_caret = caret_byte;
            self.completion = None;
        } else if !completion_standing {
            // Nothing speculative is standing, so follow the live value and caret. This is
            // what re-anchors the query segment when the user only MOVES the caret (click,
            // arrow keys) or when the caller rewrites the value between frames.
            if self.typed_text != *value {
                self.typed_text.clone_from(value);
            }
            self.typed_caret = caret_byte;
            self.completion = None;
        }
        out.changed = text_changed;

        // Matching scans every option, so only pay for it in a field the user is working in
        // (focused, or with the pointer parked on its popup).
        let resolved = if has_focus || self.keep_popup_open {
            resolve_suggestions(
                &self.typed_text,
                self.typed_caret,
                options,
                self.max_suggestions,
            )
        } else {
            None
        };

        let mut caret = CaretWriter {
            state: &mut text_output.state,
            ctx: &ctx,
            text_id,
        };

        if text_changed
            && has_focus
            && !deletion_like_change
            && caret_is_collapsed
            && let Some(list) = resolved.as_ref()
            // Completing inside a word would push a tail into the middle of it; only draw
            // when the caret sits at the end of the line or right before a break.
            && caret_ends_word(&self.typed_text, list.caret)
            && let Some((candidate, covered_chars)) = inline_candidate(&self.typed_text, list)
            // The query is the CANDIDATE's own segment: variants come from every boundary.
            && let Some(query) = self
                .typed_text
                .get(candidate.segment_start..list.caret)
            // Gates on WRITING into the caller's value only; the popup below still lists
            // everything that matched.
            && inline_insertion_allowed(query, list.suggestions.len(), list.truncated)
        {
            let spliced = splice_segment(
                &self.typed_text,
                candidate.segment_start,
                list.caret,
                candidate.insert_text(),
            );
            // The text before the segment is untouched by the splice, so its char count is
            // the same in the new value. The selection then starts after the part of the
            // INSERTION the query covers — measured on the candidate by
            // `ignore_case_prefix_len`, never as `query.chars().count()`: folding can make
            // one typed char cover several candidate chars (`İ` -> `i` + U+0307), and an
            // anchor derived from the query would then select the wrong slice, so the next
            // keystroke would overwrite the wrong text.
            let selection_start = self
                .typed_text
                .get(..candidate.segment_start)
                .map_or(0, |head| head.chars().count())
                + covered_chars;
            value.clear();
            value.push_str(&spliced.value);
            caret.set_selection(selection_start, spliced.caret_char);
            self.completion = Some(PendingCompletion {
                value: value.clone(),
                caret_char: spliced.caret_char,
                caret_byte: spliced.caret_byte,
                full: candidate.full.to_owned(),
            });
            self.last_value.clone_from(value);
            out.changed = true;
        }

        // Escape is resolved OUTSIDE the popup branch on purpose: a speculative completion
        // stands in the CALLER's value regardless of whether suggestions resolved this
        // frame, and this project's caller rebuilds its candidate list at runtime. Handling
        // the rollback only while a popup is up would leave text the user never typed in the
        // field, on its way to being saved. See `escape_action`.
        match escape_action(
            ui.input(|i| i.key_pressed(Key::Escape)),
            self.completion
                .as_ref()
                .is_some_and(|completion| completion.value == *value),
            self.highlighted_idx.is_some(),
        ) {
            EscapeAction::RestoreTypedText => {
                if self.restore_typed_text(value, &mut caret) {
                    self.highlighted_idx = None;
                    out.changed = true;
                }
            }
            EscapeAction::ClearHighlight => self.highlighted_idx = None,
            EscapeAction::ClosePopup => self.keep_popup_open = false,
            EscapeAction::Ignore => {}
        }

        let popup_open = (has_focus || self.keep_popup_open)
            && resolved
                .as_ref()
                .is_some_and(|list| !list.suggestions.is_empty());
        out.popup_open = popup_open;

        // `popup_open` implies `resolved` is `Some`; the `let` keeps that fact typed.
        if let Some(list) = resolved.as_ref().filter(|_| popup_open) {
            let count = list.suggestions.len();
            // Recomputed AFTER the Escape handling above, so an Escape in the same frame
            // cannot be followed by a Tab that accepts what Escape just rolled back.
            let inline_completion_active = self
                .completion
                .as_ref()
                .is_some_and(|completion| completion.value == *value);

            if ui.input(|i| i.key_pressed(Key::ArrowDown)) {
                let next = match self.highlighted_idx {
                    Some(idx) if idx + 1 < count => idx + 1,
                    _ => 0,
                };
                self.highlighted_idx = Some(next);
            }
            if ui.input(|i| i.key_pressed(Key::ArrowUp)) {
                let prev = match self.highlighted_idx {
                    Some(idx) if idx > 0 => idx - 1,
                    _ => count.saturating_sub(1),
                };
                self.highlighted_idx = Some(prev);
            }
            // Tab/→ accept EXACTLY what the field shows selected. If the caller's candidate
            // list changed since the completion was drawn, that text is still what the user
            // sees and asked for, so it is committed unchanged — silently swapping in the
            // new first candidate would be worse. `selected_suggestion` then names a
            // candidate that has since disappeared; reviewed and accepted, do not "fix".
            let tab_accept_pressed = if self.highlighted_idx.is_some() || inline_completion_active {
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab))
            } else {
                false
            };
            let right_accept_pressed = if inline_completion_active {
                ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowRight))
            } else {
                false
            };
            if tab_accept_pressed || right_accept_pressed {
                if let Some(idx) = self.highlighted_idx
                    && let Some(suggestion) = list.suggestions.get(idx).copied()
                {
                    self.apply_suggestion(value, list.caret, suggestion, &mut caret, &mut out, false);
                } else if inline_completion_active {
                    self.accept_completion(value, &mut caret, &mut out);
                }
                ctx.memory_mut(|mem| mem.request_focus(text_id));
            }

            if ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Some(idx) = self.highlighted_idx
                    && let Some(suggestion) = list.suggestions.get(idx).copied()
                {
                    self.apply_suggestion(value, list.caret, suggestion, &mut caret, &mut out, true);
                } else {
                    if inline_completion_active {
                        self.accept_completion(value, &mut caret, &mut out);
                    }
                    out.submitted = true;
                }
            }

            let highlighted = self.highlighted_idx;
            let mut hovered_idx = None;
            let mut clicked_idx = None;
            let popup_pos = egui::pos2(field_rect.left(), field_rect.bottom());
            let popup_width = field_rect.width();
            let popup_response = egui::Area::new(popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(popup_pos)
                .show(&ctx, |ui| {
                    ui.set_min_width(popup_width);
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(popup_width);
                        for (idx, suggestion) in list.suggestions.iter().enumerate() {
                            let label = suggestion_label(ui, *suggestion);
                            let response = ui.selectable_label(highlighted == Some(idx), label);
                            if response.hovered() {
                                hovered_idx = Some(idx);
                            }
                            if response.clicked() {
                                clicked_idx = Some(idx);
                            }
                        }
                    });
                });
            if let Some(idx) = hovered_idx {
                self.highlighted_idx = Some(idx);
            }
            self.keep_popup_open =
                popup_response.response.contains_pointer() || popup_response.response.hovered();
            if let Some(idx) = clicked_idx
                && let Some(suggestion) = list.suggestions.get(idx).copied()
            {
                self.apply_suggestion(value, list.caret, suggestion, &mut caret, &mut out, true);
                ctx.memory_mut(|mem| mem.request_focus(text_id));
                // The click already answered the popup; keep it from re-latching under the
                // pointer that is still hovering it.
                self.keep_popup_open = false;
            }
        } else {
            self.highlighted_idx = None;
            self.keep_popup_open = false;
        }

        if lost_focus && ui.input(|i| i.key_pressed(Key::Enter)) {
            out.submitted = true;
        }

        if !has_focus {
            // Symmetric counterpart of the Escape rollback: a completion may not survive
            // unfocused, or it would be neither visible as speculative nor reversible (the
            // widget no longer receives Escape). It is COMMITTED, not reverted — what the
            // user sees in the field when they click away is what they expect to keep, and
            // the caller has already been told the value changed. The snapshot re-anchors on
            // that committed text with the caret at its end.
            self.completion = None;
            if self.typed_text != *value {
                self.typed_text.clone_from(value);
            }
            self.typed_caret = value.len();
        }

        out
    }

    /// Rolls a speculative completion back to the text the user typed, restoring the caret
    /// to the position it had when the completion was drawn.
    ///
    /// Returns `false` and leaves the text alone when nothing speculative stands.
    fn restore_typed_text(&mut self, value: &mut String, caret: &mut CaretWriter<'_>) -> bool {
        self.completion = None;
        if *value == self.typed_text {
            return false;
        }
        value.clear();
        value.push_str(&self.typed_text);
        caret.set_caret(char_offset_of_byte(&self.typed_text, self.typed_caret));
        self.last_value.clone_from(value);
        true
    }

    /// Accepts the standing speculative completion: drops the selection, puts the caret just
    /// past the inserted text, and re-anchors the typed snapshot on the accepted value.
    ///
    /// Reports the FULL candidate, not the line, in `out.selected_suggestion`. Does nothing
    /// when no completion stands.
    fn accept_completion(
        &mut self,
        value: &str,
        caret: &mut CaretWriter<'_>,
        out: &mut AutocompleteLineResponse,
    ) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        caret.set_caret(completion.caret_char);
        self.typed_text.clear();
        self.typed_text.push_str(value);
        self.typed_caret = completion.caret_byte;
        self.last_value.clear();
        self.last_value.push_str(value);
        out.suggestion_applied = true;
        out.selected_suggestion = Some(completion.full);
    }

    /// Applies a popup entry: replaces ONLY the entry's OWN query segment of the typed text
    /// with its insertion, keeps the rest of the line, and moves the caret to the end of the
    /// insertion.
    ///
    /// `segment_end` is the caret position all variants share; the start comes from the
    /// chosen `suggestion`, because the list is merged from every segment boundary.
    /// `submitted` distinguishes `Enter` (submits) from `Tab` (does not).
    fn apply_suggestion(
        &mut self,
        value: &mut String,
        segment_end: usize,
        suggestion: Suggestion<'_>,
        caret: &mut CaretWriter<'_>,
        out: &mut AutocompleteLineResponse,
        submitted: bool,
    ) {
        let spliced = splice_segment(
            &self.typed_text,
            suggestion.segment_start,
            segment_end,
            suggestion.insert_text(),
        );
        value.clear();
        value.push_str(&spliced.value);
        caret.set_caret(spliced.caret_char);
        // The typed snapshot must follow the keyboard path too, otherwise the next frame
        // would still see the pre-application text and treat the accepted value as a
        // speculative completion that Escape may revert.
        self.typed_text.clone_from(value);
        self.typed_caret = spliced.caret_byte;
        self.completion = None;
        self.last_value.clone_from(value);
        self.highlighted_idx = None;
        out.changed = true;
        out.submitted = submitted;
        out.suggestion_applied = true;
        out.selected_suggestion = Some(suggestion.full.to_owned());
    }
}

/// The three values needed to write a caret position back into egui's text-edit state.
///
/// egui keeps the caret in `Context::data` keyed by the text edit's id, so moving it from
/// outside the widget means mutating the state copy and storing it again.
struct CaretWriter<'a> {
    state: &'a mut TextEditState,
    ctx: &'a egui::Context,
    text_id: Id,
}

impl CaretWriter<'_> {
    /// Stores a collapsed caret at char offset `caret`.
    fn set_caret(&mut self, caret: usize) {
        self.store(egui::text::CCursorRange::one(egui::text::CCursor::new(
            caret,
        )));
    }

    /// Stores a selection spanning char offsets `from..to`, leaving the caret at `to`
    /// (`CCursorRange::two` makes its second argument the primary cursor).
    fn set_selection(&mut self, from: usize, to: usize) {
        self.store(egui::text::CCursorRange::two(
            egui::text::CCursor::new(from),
            egui::text::CCursor::new(to),
        ));
    }

    fn store(&mut self, range: egui::text::CCursorRange) {
        self.state.cursor.set_char_range(Some(range));
        self.state.clone().store(self.ctx, self.text_id);
    }
}

/// One ranked candidate: the full option plus where inside it the inserted text begins.
///
/// `insert_from` is always a char boundary of `full`. It is `0` for a whole-candidate
/// prefix match and for the "nothing left to add" case (see [`word_boundary_insert`]);
/// otherwise it is the offset of the word that matched, so the insertion is the candidate's
/// tail from that word on.
///
/// `segment_start` is the variant's OWN segment: variants are collected from every segment
/// boundary of the typed line, so there is no single segment shared by the result. Every
/// consumer — the inline write, the popup entry, `apply_suggestion` — must splice at the
/// start carried by the variant it picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Suggestion<'a> {
    full: &'a str,
    insert_from: usize,
    segment_start: usize,
}

impl<'a> Suggestion<'a> {
    /// The text this suggestion inserts over the query segment.
    fn insert_text(&self) -> &'a str {
        self.full.get(self.insert_from..).unwrap_or(self.full)
    }

    /// True when the insertion IS the whole candidate, i.e. the popup entry needs no dimmed
    /// "…belongs to" part.
    fn inserts_whole_candidate(&self) -> bool {
        self.insert_from == 0
    }
}

/// The variants offered for one frame, ranked across ALL segment boundaries.
#[derive(Debug)]
struct SuggestionList<'a> {
    /// Byte offset in the typed text where every variant's segment ends (the caret).
    caret: usize,
    /// Ranked variants: segments CLOSEST TO THE CARET first, and inside one segment prefix
    /// matches before word-boundary matches. No two entries splice to the same line.
    suggestions: Vec<Suggestion<'a>>,
    /// A further DISTINCT variant existed but did not fit `max_suggestions`. It means
    /// "more than one variant" even when only one is listed, which blocks the speculative
    /// insertion (see [`inline_insertion_allowed`]).
    truncated: bool,
}

/// Bounded accumulator of ranked variants, filtered and deduplicated as they arrive.
///
/// Two entries are the same variant when they would produce the same LINE, not merely the
/// same inserted text: "replace `Ки` with `Ким Санхён`" and "replace the whole line with
/// `Ли Вон-джин и Ким Санхён`" are one offer reached from two segments. Counting them twice
/// would list a duplicate row and block the single-variant insertion gate.
#[derive(Debug)]
struct SuggestionSet<'a, 't> {
    items: Vec<Suggestion<'a>>,
    /// The typed line and caret every variant is measured against.
    typed: &'t str,
    caret: usize,
    max: usize,
    truncated: bool,
}

impl<'a, 't> SuggestionSet<'a, 't> {
    /// Creates an empty set for `typed`/`caret` that will hold at most `max` variants.
    fn new(typed: &'t str, caret: usize, max: usize) -> Self {
        Self {
            items: Vec::new(),
            typed,
            caret,
            max,
            truncated: false,
        }
    }

    /// Adds `suggestion` unless it adds nothing (see [`is_useless_variant`]) or an entry
    /// already splices to the same line.
    ///
    /// A distinct variant that does not fit the limit sets `truncated` instead of being
    /// dropped silently. Callers must push in rank order: the limit keeps what came first.
    fn push(&mut self, suggestion: Suggestion<'a>) {
        if is_useless_variant(self.typed, self.caret, &suggestion) {
            return;
        }
        if self
            .items
            .iter()
            .any(|existing| spliced_result_eq(self.typed, existing, &suggestion))
        {
            return;
        }
        if self.items.len() >= self.max {
            self.truncated = true;
            return;
        }
        self.items.push(suggestion);
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// True once no further candidate can change the outcome: the list is full and an extra
    /// variant has already been seen. Because pushes arrive in rank order, everything still
    /// unscanned is lower priority, so the scan may stop.
    fn is_settled(&self) -> bool {
        self.truncated && self.items.len() >= self.max
    }

    /// Freezes the set into the frame's result.
    fn into_list(self) -> SuggestionList<'a> {
        SuggestionList {
            caret: self.caret,
            suggestions: self.items,
            truncated: self.truncated,
        }
    }
}

/// Result of replacing a query segment with an insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SplicedValue {
    /// The full new line.
    value: String,
    /// Byte offset in `value` just past the inserted text.
    caret_byte: usize,
    /// Char offset in `value` just past the inserted text (egui cursors count chars).
    caret_char: usize,
}

/// What pressing `Escape` must do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeAction {
    /// Roll the speculative completion back to the text the user typed.
    RestoreTypedText,
    /// Nothing speculative stands: drop the popup highlight.
    ClearHighlight,
    /// Nothing to restore and nothing highlighted: close the popup.
    ClosePopup,
    /// `Escape` was not pressed.
    Ignore,
}

/// Decides what `Escape` does, from widget state alone.
///
/// Rollback has priority and is INDEPENDENT of the popup: the speculative completion lives
/// in the caller's value, so it must stay reversible even in a frame where no suggestion
/// resolved (the caller may rebuild its candidate list between frames). The remaining two
/// steps are the usual "dismiss the list" ladder.
///
/// The only state that can outlive this decision is a completion standing when the field
/// loses focus; `AutocompleteLine::draw` commits it there, because what the user sees in an
/// unfocused field is what they expect to keep.
#[must_use]
fn escape_action(pressed: bool, completion_stands: bool, has_highlight: bool) -> EscapeAction {
    if !pressed {
        EscapeAction::Ignore
    } else if completion_stands {
        EscapeAction::RestoreTypedText
    } else if has_highlight {
        EscapeAction::ClearHighlight
    } else {
        EscapeAction::ClosePopup
    }
}

/// True when two variants would splice `typed` into the same line.
///
/// The text after the caret is common to both, so only `typed[..segment_start]` plus the
/// insertion has to be compared. Compared as byte streams, which is UTF-8-exact and needs no
/// intermediate `String` — this runs once per candidate per already-listed variant.
#[must_use]
fn spliced_result_eq(typed: &str, a: &Suggestion<'_>, b: &Suggestion<'_>) -> bool {
    let head_a = typed.get(..a.segment_start).unwrap_or("");
    let head_b = typed.get(..b.segment_start).unwrap_or("");
    head_a
        .bytes()
        .chain(a.insert_text().bytes())
        .eq(head_b.bytes().chain(b.insert_text().bytes()))
}

/// True when a variant would add nothing to the line and must not be offered at all — not in
/// the popup and not in the variant count.
///
/// Two shapes of "nothing", both seen in live data once the caller started saving unfinished
/// input back into its own candidate list:
/// - the insertion IS the segment it replaces, case aside, so the line would come out exactly
///   as typed. Such a candidate used to win its segment outright and mask the real suggestion
///   sitting on a shorter segment;
/// - the text immediately before the segment already spells the beginning of the insertion,
///   so splicing would repeat it: `"Ли Вон-джин и "` + `"Ли Вон-джин и Ки"`. This is the same
///   stale candidate reached through the word-boundary pass, where its LAST word matches the
///   segment and the whole candidate is therefore offered.
///
/// Neither clause disturbs the legitimate "the matched word is the candidate's last one, so
/// offer the whole name" rule of [`word_boundary_insert`]: there the preceding text is
/// unrelated to the candidate, so the insertion genuinely adds the rest of the name.
#[must_use]
fn is_useless_variant(typed: &str, caret: usize, suggestion: &Suggestion<'_>) -> bool {
    let insert = suggestion.insert_text();
    let segment = typed.get(suggestion.segment_start..caret).unwrap_or("");
    if eq_ignore_case(insert, segment) {
        return true;
    }
    // Trailing whitespace of the head is the separator the user already typed, not content.
    let head = typed
        .get(..suggestion.segment_start)
        .unwrap_or("")
        .trim_end();
    !head.is_empty() && starts_with_ignore_case(insert, head)
}

/// True when `option` has more than [`MAX_COMPLETION_CANDIDATE_WORDS`] whitespace-separated
/// parts, i.e. it is a descriptive line rather than a name.
///
/// `split_whitespace` collapses runs of whitespace, so "а  б" is two parts, not three, and
/// leading or trailing spaces add none.
#[must_use]
fn is_long_candidate(option: &str) -> bool {
    option.split_whitespace().count() > MAX_COMPLETION_CANDIDATE_WORDS
}

/// Number of non-whitespace chars in `text`.
///
/// The "how much has the user actually committed to" measure: "Ин С" counts as 3, a lone
/// "и" as 1, and a segment of spaces as 0.
#[must_use]
fn significant_char_count(text: &str) -> usize {
    text.chars().filter(|ch| !ch.is_whitespace()).count()
}

/// Whether the widget may WRITE a speculative completion for this segment.
///
/// Two independent gates, both learned from live use, and neither of them touches the
/// popup — listing options costs the user nothing, silently rewriting their text does:
/// - the segment must hold at least [`MIN_INLINE_SIGNIFICANT_CHARS`] non-whitespace chars,
///   so a one-letter conjunction cannot pull in a name and impose its capitalisation;
/// - exactly one variant must remain. While two or more insertions are possible the widget
///   has not actually guessed anything, so it lists them and waits.
///
/// `truncated` says a further DISTINCT variant existed but did not fit the display limit;
/// that is still "more than one", so it blocks the insertion too.
#[must_use]
fn inline_insertion_allowed(query: &str, variant_count: usize, truncated: bool) -> bool {
    significant_char_count(query) >= MIN_INLINE_SIGNIFICANT_CHARS
        && variant_count == 1
        && !truncated
}

/// True for characters that terminate an autocomplete segment: any whitespace plus
/// [`SEGMENT_SEPARATORS`].
fn is_segment_break(ch: char) -> bool {
    ch.is_whitespace() || SEGMENT_SEPARATORS.contains(&ch)
}

/// Byte offsets at which a segment may start inside `text[..caret]`, ordered from the
/// EARLIEST (longest segment) to the latest (shortest).
///
/// A segment starts at the beginning of the line and right after every segment break, so
/// "Кон Че Мин или Ин" offers the segments "Кон Че Мин или Ин", "Че Мин или Ин",
/// "Мин или Ин", "или Ин" and "Ин". An offset equal to `caret` (an empty segment) and an
/// offset landing on another break (a whitespace-led query that can never match a
/// candidate) are not produced. Returns an empty vector when `caret` is not a valid
/// boundary of `text`.
fn segment_starts(text: &str, caret: usize) -> Vec<usize> {
    let Some(head) = text.get(..caret) else {
        return Vec::new();
    };
    let mut starts = vec![0];
    for (offset, ch) in head.char_indices() {
        if !is_segment_break(ch) {
            continue;
        }
        let next = offset + ch.len_utf8();
        if next >= caret {
            continue;
        }
        // A run of breaks ("a,  b") must contribute exactly one start — the one on the
        // real character — otherwise every extra start only costs a scan that cannot match.
        if head
            .get(next..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(is_segment_break)
        {
            continue;
        }
        starts.push(next);
    }
    starts
}

/// Byte offsets inside `text` at which a WORD starts: `0` plus every offset right after a
/// segment break.
///
/// Used to reject a substring match that lands inside a word — otherwise typing "ин" would
/// offer the tail of "Лин". May yield `text.len()` for a text ending in a break; the empty
/// tail simply matches nothing.
fn word_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        text.char_indices()
            .filter(|(_, ch)| is_segment_break(*ch))
            .map(|(offset, ch)| offset + ch.len_utf8()),
    )
}

/// Case-insensitive prefix test that also reports HOW MUCH of `haystack` the prefix covers.
///
/// Returns `Some(n)` when `needle` is a case-insensitive prefix of `haystack`, `n` being the
/// number of `haystack` CHARS the needle consumed; `None` when it is not a prefix.
///
/// `n` is deliberately not `needle.chars().count()`: full case folding may map one char to
/// several (`İ` -> `i` + U+0307), so a query can cover more chars of the candidate than it
/// contains itself. Any boundary drawn INSIDE the candidate — above all the inline selection
/// anchor — must come from this count, or the selection starts in the wrong place. A
/// haystack char whose folding is only partially consumed counts as consumed, because a char
/// index cannot point inside a char.
///
/// Comparison streams lowercased chars instead of allocating lowercase copies: an offset
/// found in a lowercased copy cannot be reused on the original string, and streaming
/// short-circuits on the first mismatch, which is what the per-frame candidate scan does
/// most of the time.
///
/// `pub(super)` so `searchable_combo_box.rs` scans for substring matches with the SAME
/// folding rules instead of growing a second, subtly different implementation of them.
#[must_use]
pub(super) fn ignore_case_prefix_len(haystack: &str, needle: &str) -> Option<usize> {
    let mut hay_chars = haystack.chars();
    let mut hay_folded: Option<std::char::ToLowercase> = None;
    let mut consumed = 0usize;
    for expected in needle.chars().flat_map(char::to_lowercase) {
        let actual = loop {
            if let Some(folded) = hay_folded.as_mut()
                && let Some(ch) = folded.next()
            {
                break ch;
            }
            let next = hay_chars.next()?;
            consumed += 1;
            hay_folded = Some(next.to_lowercase());
        };
        if actual != expected {
            return None;
        }
    }
    Some(consumed)
}

/// Case-insensitive `str::starts_with`; see [`ignore_case_prefix_len`] for the contract.
#[must_use]
fn starts_with_ignore_case(haystack: &str, needle: &str) -> bool {
    ignore_case_prefix_len(haystack, needle).is_some()
}

/// Case-insensitive string equality with the same streaming contract as
/// [`starts_with_ignore_case`].
fn eq_ignore_case(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Clamps a byte offset into `text` down to the nearest char boundary at or below it.
///
/// Keeps every slicing helper total: a caret that went out of sync degrades to a shorter
/// query instead of panicking.
fn clamp_to_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Number of chars in `text` before byte offset `byte_idx`.
fn char_offset_of_byte(text: &str, byte_idx: usize) -> usize {
    text.char_indices()
        .take_while(|(offset, _)| *offset < byte_idx)
        .count()
}

/// Byte offset of char number `char_idx` in `text`, or `text.len()` at or past the end.
fn byte_offset_of_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map_or(text.len(), |(offset, _)| offset)
}

/// Finds where `query` matches `option` at a WORD boundary and returns the byte offset the
/// insertion should start at.
///
/// A match strictly inside a word is rejected. When the matched word is the candidate's
/// last one and its tail EQUALS the query, inserting that tail would change nothing, so the
/// whole candidate is inserted instead (`0` is returned) — that keeps every popup entry
/// actionable ("Лин" offers "Ин Су Лин", not a no-op).
///
/// Returns `None` when the query matches no word boundary.
fn word_boundary_insert(option: &str, query: &str) -> Option<usize> {
    for start in word_starts(option) {
        let Some(tail) = option.get(start..) else {
            continue;
        };
        if tail.is_empty() || !starts_with_ignore_case(tail, query) {
            continue;
        }
        return Some(if eq_ignore_case(tail, query) { 0 } else { start });
    }
    None
}

/// Adds candidates whose whole text starts with `query` (case-insensitively), in option
/// order.
///
/// `allow_long_candidates` admits candidates over [`MAX_COMPLETION_CANDIDATE_WORDS`] parts.
/// It is true only when the segment starts at offset 0, i.e. the user is typing the field
/// from scratch and a long descriptive row is a plausible thing to be naming.
fn collect_prefix_matches<'a, S: AsRef<str>>(
    set: &mut SuggestionSet<'a, '_>,
    query: &str,
    segment_start: usize,
    options: &'a [S],
    allow_long_candidates: bool,
) {
    for option in options {
        if set.is_settled() {
            return;
        }
        let full = option.as_ref();
        if !allow_long_candidates && is_long_candidate(full) {
            continue;
        }
        if starts_with_ignore_case(full, query) {
            set.push(Suggestion {
                full,
                insert_from: 0,
                segment_start,
            });
        }
    }
}

/// Adds candidates matching `query` at a word boundary, inserting the candidate's tail.
///
/// Long candidates are excluded unconditionally here: a tail like "и Пак Соэ по телефону"
/// is not a name and is never a useful thing to splice into a line, at any position.
fn collect_word_matches<'a, S: AsRef<str>>(
    set: &mut SuggestionSet<'a, '_>,
    query: &str,
    segment_start: usize,
    options: &'a [S],
) {
    for option in options {
        if set.is_settled() {
            return;
        }
        let full = option.as_ref();
        if is_long_candidate(full) {
            continue;
        }
        if let Some(insert_from) = word_boundary_insert(full, query) {
            set.push(Suggestion {
                full,
                insert_from,
                segment_start,
            });
        }
    }
}

/// Collects the variants offered for `typed[..caret]` from EVERY segment boundary.
///
/// No segment "wins": the lists of all segments are merged. A single winner meant that one
/// stale candidate prefix-matching the whole line could take the win and hide the real
/// completion of the two letters the user was typing at the caret.
///
/// Ranking is by proximity to the caret — shortest segment first, then longer ones — because
/// the user is completing what they are typing right now, not the sentence leading up to it.
/// Inside one segment the order stays prefix matches before word-boundary matches.
/// Duplicates and useless variants are dropped as they arrive (see [`SuggestionSet::push`]),
/// and `max_suggestions` therefore keeps the highest-ranked ones.
///
/// Long candidates are filtered out INSIDE each per-segment scan, before anything is
/// collected. Filtering afterwards would let a descriptive row occupy the list on a long
/// segment and push the short meaningful match past the limit.
///
/// `caret` is a byte offset into `typed` and is clamped to a char boundary, so an
/// out-of-sync caret degrades to fewer suggestions instead of panicking. Empty and
/// whitespace-only segments are skipped.
///
/// Returns `None` when nothing matches.
fn resolve_suggestions<'a, S: AsRef<str>>(
    typed: &str,
    caret: usize,
    options: &'a [S],
    max_suggestions: usize,
) -> Option<SuggestionList<'a>> {
    if options.is_empty() || max_suggestions == 0 {
        return None;
    }
    let caret = clamp_to_char_boundary(typed, caret);
    let starts = segment_starts(typed, caret);
    let mut set = SuggestionSet::new(typed, caret, max_suggestions);

    // `segment_starts` yields longest-segment-first; walk it backwards to rank the segment
    // nearest the caret highest. Stopping early is safe only because of that order.
    for &segment_start in starts.iter().rev() {
        if set.is_settled() {
            break;
        }
        let Some(query) = typed.get(segment_start..caret) else {
            continue;
        };
        if query.trim().is_empty() {
            continue;
        }
        // A long candidate may be completed only when the whole field is being typed from
        // its start; mid-line it is not a name the user is naming.
        collect_prefix_matches(&mut set, query, segment_start, options, segment_start == 0);
        collect_word_matches(&mut set, query, segment_start, options);
    }

    (!set.is_empty()).then(|| set.into_list())
}

/// Replaces `typed[segment_start..caret]` with `insert`, keeping the rest of the line.
///
/// Both offsets are clamped to char boundaries of `typed` (and `segment_start` to `caret`),
/// so the function is total. Returns the new line plus the position just past the insertion
/// in both bytes and chars — egui cursors count chars, the typed snapshot counts bytes.
fn splice_segment(typed: &str, segment_start: usize, caret: usize, insert: &str) -> SplicedValue {
    let caret = clamp_to_char_boundary(typed, caret);
    let segment_start = clamp_to_char_boundary(typed, segment_start.min(caret));
    let head = typed.get(..segment_start).unwrap_or("");
    let tail = typed.get(caret..).unwrap_or("");
    let mut value = String::with_capacity(head.len() + insert.len() + tail.len());
    value.push_str(head);
    value.push_str(insert);
    // Measured before the tail is appended, which is exactly "just past the insertion".
    let caret_byte = value.len();
    let caret_char = value.chars().count();
    value.push_str(tail);
    SplicedValue {
        value,
        caret_byte,
        caret_char,
    }
}

/// How many chars of `insert` the `query` already covers, when `insert` may be drawn inline.
///
/// `Some(covered)` only when `insert` CONTINUES the query: it starts with it
/// (case-insensitively) and adds at least one char beyond what the query covers, so the
/// drawn-and-selected tail is exactly what accepting keeps. An "insert the whole candidate"
/// entry does not continue the query and is rejected here.
///
/// `covered` is the inline selection anchor. It comes from [`ignore_case_prefix_len`] and NOT
/// from `query.chars().count()`, because folding may make the query cover a different number
/// of candidate chars than it has itself.
///
/// Case follows the CANDIDATE, not the typed text (typing "су" yields "Су Лин"): the typed
/// part is inside the replaced range anyway, and the canonical spelling is what the caller
/// stores.
#[must_use]
fn inline_coverage(insert: &str, query: &str) -> Option<usize> {
    let covered = ignore_case_prefix_len(insert, query)?;
    (insert.chars().count() > covered).then_some(covered)
}

/// Picks the highest-ranked variant that may be drawn INLINE, with its coverage.
///
/// Each variant is tested against ITS OWN segment, because the list is merged from every
/// segment boundary. Ranking already puts the segment nearest the caret first, so the first
/// variant that qualifies is the one the user is typing.
#[must_use]
fn inline_candidate<'a>(typed: &str, list: &SuggestionList<'a>) -> Option<(Suggestion<'a>, usize)> {
    list.suggestions.iter().copied().find_map(|suggestion| {
        let query = typed.get(suggestion.segment_start..list.caret)?;
        inline_coverage(suggestion.insert_text(), query).map(|covered| (suggestion, covered))
    })
}

/// True when an inline completion may be drawn at `caret`: the character right after the
/// caret must end the word, otherwise the user is editing INSIDE a word and a speculative
/// tail would land in the middle of it.
fn caret_ends_word(typed: &str, caret: usize) -> bool {
    typed
        .get(caret..)
        .is_none_or(|tail| tail.chars().next().is_none_or(is_segment_break))
}

/// Builds the popup entry for a suggestion.
///
/// An entry whose insertion is the whole candidate renders as plain text. A tail insertion
/// renders as the inserted text followed, after a gap, by the full candidate in the weak
/// color, so the user sees which name the tail belongs to. The inserted part is painted
/// with `Color32::PLACEHOLDER` (`ecolor-0.35.0/src/color32.rs:104`), which egui substitutes
/// with the label's own selected/hovered text color at paint time
/// (`egui-0.35.0/src/atomics/atom_layout.rs:669`).
fn suggestion_label(ui: &egui::Ui, suggestion: Suggestion<'_>) -> egui::WidgetText {
    let insert = suggestion.insert_text();
    if suggestion.inserts_whole_candidate() {
        return insert.to_owned().into();
    }
    // `Button::selectable` falls back to `TextStyle::Button`; an explicit `LayoutJob` must
    // resolve the same font itself. Missing style entries fall back instead of panicking.
    let font_id = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Button)
        .cloned()
        .unwrap_or_default();
    let mut job = egui::text::LayoutJob::default();
    job.append(
        insert,
        0.0,
        egui::TextFormat::simple(font_id.clone(), egui::Color32::PLACEHOLDER),
    );
    job.append(
        suggestion.full,
        SUGGESTION_HINT_GAP,
        egui::TextFormat::simple(font_id, ui.visuals().weak_text_color()),
    );
    job.into()
}

/// True when the edit that just happened looks like a deletion, in which case no inline
/// completion may be drawn (re-adding the tail the user just erased would trap them).
fn is_deletion_like_change(ui: &egui::Ui, before_edit: &str, after_edit: &str) -> bool {
    let before_len = before_edit.chars().count();
    let after_len = after_edit.chars().count();
    if after_len < before_len {
        return true;
    }

    ui.input(|i| {
        i.key_pressed(Key::Backspace)
            || i.key_pressed(Key::Delete)
            || i.events.iter().any(|ev| matches!(ev, egui::Event::Cut))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A descriptive row label, not a name: 7 whitespace-separated parts. Taken verbatim
    /// from the report that produced the long-candidate rule.
    const LONG_ROW: &str = "Разговор Ын Соён и Пак Соэ по телефону";

    /// Convenience: resolve with the caret at the end and the default limit.
    fn resolve_at_end<'a>(typed: &str, options: &'a [&str]) -> Option<SuggestionList<'a>> {
        resolve_suggestions(typed, typed.len(), options, 8)
    }

    /// Convenience: resolve and return the TOP variant as
    /// `(its segment_start, its query, all insert texts)`.
    fn resolve<'a>(typed: &'a str, options: &[&str]) -> Option<(usize, &'a str, Vec<String>)> {
        let resolved = resolve_at_end(typed, options)?;
        let top = *resolved.suggestions.first()?;
        let query = &typed[top.segment_start..resolved.caret];
        Some((top.segment_start, query, inserts_of(&resolved)))
    }

    /// Convenience: the insert texts of every variant, in rank order.
    fn inserts_of(resolved: &SuggestionList<'_>) -> Vec<String> {
        resolved
            .suggestions
            .iter()
            .map(|s| s.insert_text().to_owned())
            .collect()
    }

    /// Convenience: the line each variant would produce, in rank order.
    fn spliced_of(typed: &str, resolved: &SuggestionList<'_>) -> Vec<String> {
        resolved
            .suggestions
            .iter()
            .map(|s| {
                splice_segment(typed, s.segment_start, resolved.caret, s.insert_text()).value
            })
            .collect()
    }

    #[test]
    fn segment_starts_walk_from_longest_to_shortest() {
        let text = "Кон Че Мин или Ин";
        let starts = segment_starts(text, text.len());
        let segments: Vec<&str> = starts.iter().map(|&s| &text[s..]).collect();
        assert_eq!(
            segments,
            vec![
                "Кон Че Мин или Ин",
                "Че Мин или Ин",
                "Мин или Ин",
                "или Ин",
                "Ин",
            ]
        );
    }

    #[test]
    fn segment_starts_break_on_punctuation_but_not_on_hyphen() {
        let text = "«Ким-Чан, Ин";
        let starts = segment_starts(text, text.len());
        let segments: Vec<&str> = starts.iter().map(|&s| &text[s..]).collect();
        // `«` and `,` break; `-` does not, so "Ким-Чан" stays one segment.
        assert_eq!(segments, vec!["«Ким-Чан, Ин", "Ким-Чан, Ин", "Ин"]);
    }

    #[test]
    fn segment_starts_skip_the_empty_segment_at_the_caret() {
        let text = "Ин ";
        let starts = segment_starts(text, text.len());
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn segment_starts_reject_a_caret_inside_a_char() {
        // 1 is inside the two-byte "И": no panic, no starts.
        assert!(segment_starts("Ин", 1).is_empty());
    }

    #[test]
    fn shortest_matching_segment_wins_when_longer_ones_match_nothing() {
        let options = ["Кон Че Мин", "Ин Су Лин"];
        let typed = "Кон Че Мин или Ин";
        let (start, query, inserts) = resolve(typed, &options).expect("a suggestion");
        assert_eq!(query, "Ин");
        assert_eq!(inserts, vec!["Ин Су Лин".to_owned()]);

        let spliced = splice_segment(typed, start, typed.len(), &inserts[0]);
        assert_eq!(spliced.value, "Кон Че Мин или Ин Су Лин");
        assert_eq!(spliced.caret_char, spliced.value.chars().count());
    }

    #[test]
    fn a_name_reachable_from_two_segments_is_offered_once() {
        // Rewritten when variants started being collected from EVERY boundary. "Ин Су" is
        // reachable both as the whole line (prefix match, insert the whole name) and as the
        // segment "Су" (word-boundary match, insert "Су Лин"). Both splice to the same line,
        // so they are ONE variant — and the higher-ranked one is now the segment nearest the
        // caret, which is why the reported insert is the tail, not the whole name. The line
        // the user ends up with is unchanged, and R2 still sees a single variant.
        let options = ["Ин Су Лин"];
        let typed = "Ин Су";
        let (start, query, inserts) = resolve(typed, &options).expect("a suggestion");
        assert_eq!((start, query), ("Ин ".len(), "Су"));
        assert_eq!(inserts, vec!["Су Лин".to_owned()]);

        let resolved = resolve_at_end(typed, &options).expect("a suggestion");
        assert_eq!(spliced_of(typed, &resolved), vec!["Ин Су Лин".to_owned()]);
        assert!(inline_insertion_allowed(
            query,
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn a_word_boundary_match_inserts_the_candidate_tail() {
        let options = ["Ин Су Лин"];
        let (start, query, inserts) = resolve("Су", &options).expect("a suggestion");
        assert_eq!((start, query), (0, "Су"));
        assert_eq!(inserts, vec!["Су Лин".to_owned()]);
    }

    #[test]
    fn a_tail_equal_to_the_query_inserts_the_whole_candidate() {
        let options = ["Ин Су Лин"];
        // "Лин" is the last word: its tail IS the query, so the entry must offer the name.
        let (_, query, inserts) = resolve("Лин", &options).expect("a suggestion");
        assert_eq!(query, "Лин");
        assert_eq!(inserts, vec!["Ин Су Лин".to_owned()]);
        assert_eq!(word_boundary_insert("Ин Су Лин", "Лин"), Some(0));
    }

    #[test]
    fn a_match_inside_a_word_is_not_a_suggestion() {
        let options = ["Кон Че Мин"];
        // "он" occurs inside "Кон" but starts no word.
        assert!(resolve("он", &options).is_none());
        assert_eq!(word_boundary_insert("Кон Че Мин", "он"), None);
    }

    #[test]
    fn prefix_matches_rank_before_word_boundary_matches() {
        let options = ["Су Джин", "Ин Су Лин"];
        let (_, query, inserts) = resolve("Су", &options).expect("a suggestion");
        assert_eq!(query, "Су");
        assert_eq!(
            inserts,
            vec!["Су Джин".to_owned(), "Су Лин".to_owned()],
            "the prefix match must come first, the tail insertion after it"
        );
    }

    #[test]
    fn the_suggestion_list_honours_the_limit() {
        let options = ["Ин А", "Ин Б", "Ин В"];
        let resolved = resolve_suggestions("Ин", 4, &options, 2).expect("suggestions");
        assert_eq!(resolved.suggestions.len(), 2);
    }

    #[test]
    fn matching_is_case_insensitive_for_cyrillic() {
        assert!(starts_with_ignore_case("Су Лин", "су"));
        assert!(starts_with_ignore_case("СУ ЛИН", "су л"));
        assert!(!starts_with_ignore_case("Су Лин", "си"));
        assert!(eq_ignore_case("ЛИН", "лин"));
        assert!(!eq_ignore_case("Лин", "Лина"));

        let options = ["Ин Су Лин"];
        let (_, _, inserts) = resolve("су", &options).expect("a suggestion");
        // Case follows the candidate, not the typed text.
        assert_eq!(inserts, vec!["Су Лин".to_owned()]);
    }

    #[test]
    fn byte_and_char_offsets_convert_both_ways_on_cyrillic() {
        let text = "Ин Су";
        assert_eq!(byte_offset_of_char(text, 0), 0);
        assert_eq!(byte_offset_of_char(text, 3), 5); // "Ин " is 5 bytes
        assert_eq!(byte_offset_of_char(text, 99), text.len());
        assert_eq!(char_offset_of_byte(text, 0), 0);
        assert_eq!(char_offset_of_byte(text, 5), 3);
        assert_eq!(char_offset_of_byte(text, text.len()), 5);
    }

    #[test]
    fn splice_keeps_the_line_around_the_segment() {
        let typed = "Ин, привет";
        let caret = "Ин".len();
        let spliced = splice_segment(typed, 0, caret, "Ин Су Лин");
        assert_eq!(spliced.value, "Ин Су Лин, привет");
        assert_eq!(spliced.caret_byte, "Ин Су Лин".len());
        assert_eq!(spliced.caret_char, 9);
    }

    #[test]
    fn splice_clamps_broken_offsets_instead_of_panicking() {
        let typed = "Ин";
        // Both offsets land inside a two-byte char and in the middle of nowhere.
        let spliced = splice_segment(typed, 1, 99, "Ин Су Лин");
        assert_eq!(spliced.value, "Ин Су Лин");
    }

    #[test]
    fn a_caret_inside_a_word_blocks_the_inline_draw() {
        assert!(caret_ends_word("Ин Су", "Ин Су".len())); // end of the line
        assert!(caret_ends_word("Ин Су Лин", 4)); // right before the space
        assert!(!caret_ends_word("Ин Су", 5)); // start of the next word
        assert!(!caret_ends_word("Инна", 2)); // inside the word
        assert!(caret_ends_word("Ин, привет", 4)); // right before the comma
    }

    #[test]
    fn only_a_continuing_and_longer_insertion_is_drawn_inline() {
        assert_eq!(inline_coverage("Су Лин", "Су"), Some(2));
        // The whole-candidate entry offered for "Лин" does not continue the query.
        assert_eq!(inline_coverage("Ин Су Лин", "Лин"), None);
        // Nothing to add: the insertion equals the query.
        assert_eq!(inline_coverage("Ин Су Лин", "Ин Су Лин"), None);
        // ...and it is picked out of a real list against its OWN segment.
        let typed = "Ин Су";
        let resolved = resolve_at_end(typed, &["Ин Су Лин"]).expect("a suggestion");
        let (candidate, covered) = inline_candidate(typed, &resolved).expect("an inline draw");
        assert_eq!(candidate.segment_start, "Ин ".len());
        assert_eq!((candidate.insert_text(), covered), ("Су Лин", 2));
    }

    #[test]
    fn the_prefix_length_counts_candidate_chars_not_query_chars() {
        // Control: a plain Cyrillic query covers exactly as many candidate chars as it has.
        assert_eq!(ignore_case_prefix_len("Су Лин", "су"), Some(2));
        assert_eq!(ignore_case_prefix_len("СУ ЛИН", "су л"), Some(4));
        assert_eq!(ignore_case_prefix_len("Су Лин", "си"), None);
        assert_eq!(ignore_case_prefix_len("Су", "Су Лин"), None);
        assert_eq!(ignore_case_prefix_len("Су Лин", ""), Some(0));

        // Length-changing folding: `İ` (U+0130) lowercases to `i` + U+0307, so one typed
        // char covers TWO chars of a decomposed candidate. Guard the premise first, so this
        // test fails loudly rather than silently passing if the mapping ever changes.
        let folded: String = 'İ'.to_lowercase().collect();
        assert_eq!(folded, "i\u{307}", "premise: `İ` folds to two chars");
        assert_eq!(ignore_case_prefix_len("i\u{307}n", "İ"), Some(2));
        // ... and the anchor derived from the query alone would have been 1, selecting
        // "\u{307}n" instead of "n".
        assert_ne!("İ".chars().count(), 2);

        // The same, through the inline coverage helper: the selection anchor must be 2.
        assert_eq!(inline_coverage("i\u{307}n", "İ"), Some(2));

        // A partially consumed candidate char still counts as consumed: a char index cannot
        // point inside a char.
        assert_eq!(ignore_case_prefix_len("İn", "i"), Some(1));
    }

    #[test]
    fn one_significant_char_lists_options_but_never_writes_them() {
        // The whole point of the gate: the popup still resolves...
        let resolved = resolve_at_end("И", &["Ируха"]).expect("a suggestion for the popup");
        assert_eq!(inserts_of(&resolved), vec!["Ируха".to_owned()]);
        // ...but nothing may be written into the caller's value.
        assert!(!inline_insertion_allowed("И", resolved.suggestions.len(), resolved.truncated));

        let resolved = resolve_at_end("Ир", &["Ируха"]).expect("a suggestion");
        assert!(inline_insertion_allowed(
            "Ир",
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn only_non_whitespace_chars_count_towards_the_write_threshold() {
        assert_eq!(significant_char_count("Ин С"), 3);
        assert_eq!(significant_char_count("И "), 1);
        assert_eq!(significant_char_count("   "), 0);
        assert_eq!(significant_char_count(""), 0);

        assert!(inline_insertion_allowed("Ин С", 1, false));
        assert!(!inline_insertion_allowed("И ", 1, false));
        assert!(!inline_insertion_allowed("  ", 1, false));
    }

    #[test]
    fn a_write_needs_exactly_one_surviving_variant() {
        assert!(inline_insertion_allowed("Су", 1, false));
        assert!(!inline_insertion_allowed("Су", 2, false));
        assert!(!inline_insertion_allowed("Су", 0, false));
        // One listed, but another distinct variant did not fit the limit.
        assert!(!inline_insertion_allowed("Су", 1, true));

        // End to end: two candidates that both prefix-match block the write.
        let resolved = resolve_at_end("Су", &["Су Джин", "Су Мин"]).expect("suggestions");
        assert_eq!(resolved.suggestions.len(), 2);
        assert!(!inline_insertion_allowed("Су", resolved.suggestions.len(), resolved.truncated));
    }

    #[test]
    fn candidates_are_long_past_four_whitespace_separated_parts() {
        assert!(!is_long_candidate("Ким Ли Со Ён"));
        assert!(is_long_candidate("Ким Ли Со Ён Хо"));
        assert!(is_long_candidate(LONG_ROW));
        // Runs of whitespace do not invent parts.
        assert!(!is_long_candidate("  Ким   Ли  Со   Ён  "));
        assert!(!is_long_candidate(""));
    }

    #[test]
    fn a_long_candidate_completes_only_from_the_start_of_the_field() {
        // Typing the field from scratch: the prefix pass still offers it.
        let resolved = resolve_at_end("Разговор", &[LONG_ROW]).expect("a prefix suggestion");
        assert_eq!(resolved.suggestions[0].segment_start, 0);
        assert_eq!(inserts_of(&resolved), vec![LONG_ROW.to_owned()]);

        // Same query, but mid-line: the segment does not start at 0, so it is out.
        assert!(resolve_at_end("Имя Разговор", &[LONG_ROW]).is_none());
        // And it never contributes a tail, at any position.
        assert!(resolve_at_end("Пак", &[LONG_ROW]).is_none());
        assert!(resolve_at_end("Имя Пак", &[LONG_ROW]).is_none());
    }

    #[test]
    fn a_four_part_candidate_still_participates_everywhere() {
        let four = "Ким Ли Со Ён";
        let options = [four];
        // Mid-line prefix match.
        let resolved = resolve_at_end("Имя Ким", &options).expect("a prefix suggestion");
        assert_eq!(inserts_of(&resolved), vec![four.to_owned()]);
        // Mid-line word-boundary match.
        let resolved = resolve_at_end("Имя Со", &options).expect("a tail suggestion");
        assert_eq!(inserts_of(&resolved), vec!["Со Ён".to_owned()]);
    }

    #[test]
    fn long_candidates_are_dropped_before_the_segment_is_chosen() {
        // Without the filter the segment "Пак Со" would match the long row's tail and win at
        // offset 0, hiding the short, meaningful match the next segment has.
        let resolved = resolve_at_end("Пак Со", &["Ким Соён", LONG_ROW]).expect("a suggestion");
        assert_eq!(resolved.suggestions[0].segment_start, "Пак ".len());
        assert_eq!(inserts_of(&resolved), vec!["Соён".to_owned()]);
    }

    #[test]
    fn variants_inserting_the_same_text_collapse_into_one() {
        // Two distinct candidates, one insertion — the popup showed these as duplicate rows.
        let resolved = resolve_at_end("Су", &["Ким Су Лин", "Пак Су Лин"]).expect("a suggestion");
        assert_eq!(inserts_of(&resolved), vec!["Су Лин".to_owned()]);
        assert!(!resolved.truncated);
        // Counted once, so the duplicate does not block the write either.
        assert!(inline_insertion_allowed(
            "Су",
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn the_reported_conjunction_case_writes_nothing() {
        // Live report: `и` is a conjunction here, not a name. Neither candidate may reach
        // the caller's value — the short one because one char is not enough evidence, the
        // long one because it is a descriptive row.
        let typed = "Ли Вон-джин И";
        let resolved = resolve_at_end(typed, &["Ируха", LONG_ROW]).expect("popup suggestions");
        let query = &typed[resolved.suggestions[0].segment_start..resolved.caret];
        assert_eq!(query, "И");
        assert_eq!(inserts_of(&resolved), vec!["Ируха".to_owned()]);
        assert!(
            !inline_insertion_allowed(query, resolved.suggestions.len(), resolved.truncated),
            "a single conjunction must never overwrite the typed text or its case"
        );
        assert!(
            !inserts_of(&resolved).iter().any(|i| i.contains("Пак")),
            "the long row must not offer a tail phrase"
        );
    }

    #[test]
    fn a_variant_that_adds_nothing_is_dropped() {
        // Clause 1: the insertion IS the segment. This is the caller's own unfinished input,
        // saved back into the candidate list.
        let stale = "Ли Вон-джин и Ки";
        assert!(is_useless_variant(
            stale,
            stale.len(),
            &Suggestion {
                full: stale,
                insert_from: 0,
                segment_start: 0,
            }
        ));
        // Clause 2: the same candidate reached from its own last word, where splicing would
        // repeat the head that is already in the line.
        assert!(is_useless_variant(
            stale,
            stale.len(),
            &Suggestion {
                full: stale,
                insert_from: 0,
                segment_start: "Ли Вон-джин и ".len(),
            }
        ));
    }

    #[test]
    fn the_whole_candidate_fallback_survives_a_mid_line_segment() {
        // The other branch, deliberately NOT caught by `is_useless_variant`: the matched word
        // is the candidate's last one, the preceding text is unrelated to it, so offering the
        // whole name really does add the rest of it.
        let typed = "Кто-то сказал Лин";
        let suggestion = Suggestion {
            full: "Ин Су Лин",
            insert_from: 0,
            segment_start: "Кто-то сказал ".len(),
        };
        assert!(!is_useless_variant(typed, typed.len(), &suggestion));
        let spliced = splice_segment(
            typed,
            suggestion.segment_start,
            typed.len(),
            suggestion.insert_text(),
        );
        assert_eq!(spliced.value, "Кто-то сказал Ин Су Лин");
    }

    #[test]
    fn a_stale_whole_line_candidate_no_longer_masks_the_real_one() {
        // Live report. `Ли Вон-джин и Ки` is the user's own half-typed value, saved into the
        // chapter's name list by the debounced write. It prefix-matches the whole line, which
        // used to win the segment outright and hide `Ким Санхён` until an `м` broke the match.
        let typed = "Ли Вон-джин и Ки";
        let options = ["Ли Вон-джин", "Ким Санхён", "Ли Вон-джин и Ки"];
        let resolved = resolve_at_end(typed, &options).expect("a suggestion");
        assert_eq!(inserts_of(&resolved), vec!["Ким Санхён".to_owned()]);
        assert_eq!(
            resolved.suggestions[0].segment_start,
            "Ли Вон-джин и ".len()
        );
        assert_eq!(
            spliced_of(typed, &resolved),
            vec!["Ли Вон-джин и Ким Санхён".to_owned()]
        );
        let query = &typed[resolved.suggestions[0].segment_start..resolved.caret];
        assert_eq!(query, "Ки");
        assert!(inline_insertion_allowed(
            query,
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn one_char_earlier_the_same_line_lists_but_does_not_write() {
        let typed = "Ли Вон-джин и К";
        let options = ["Ли Вон-джин", "Ким Санхён", "Ли Вон-джин и Ки"];
        let resolved = resolve_at_end(typed, &options).expect("a suggestion");
        assert!(
            inserts_of(&resolved).contains(&"Ким Санхён".to_owned()),
            "the name must still be offered in the popup"
        );
        // One significant char in the segment: R1 forbids touching the caller's value.
        let query = &typed[resolved.suggestions[0].segment_start..resolved.caret];
        assert_eq!(query, "К");
        assert!(!inline_insertion_allowed(
            query,
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn a_partial_second_word_still_completes() {
        // R6 must not eat a useful variant: the insertion is longer than what was typed.
        let typed = "Ким Сан";
        let resolved = resolve_at_end(typed, &["Ким Санхён"]).expect("a suggestion");
        assert_eq!(inserts_of(&resolved), vec!["Санхён".to_owned()]);
        assert_eq!(spliced_of(typed, &resolved), vec!["Ким Санхён".to_owned()]);
        let query = &typed[resolved.suggestions[0].segment_start..resolved.caret];
        assert!(inline_insertion_allowed(
            query,
            resolved.suggestions.len(),
            resolved.truncated
        ));
    }

    #[test]
    fn variants_are_ranked_from_the_segment_nearest_the_caret() {
        // Two independent names, one matched by the long segment and one by the short one.
        // The short segment is what the user is typing, so it must come first.
        let typed = "Ким Со";
        let resolved = resolve_at_end(typed, &["Ким Соён и Ко", "Соэ Пак"]).expect("suggestions");
        assert_eq!(
            spliced_of(typed, &resolved),
            vec![
                "Ким Соэ Пак".to_owned(),
                "Ким Соён и Ко".to_owned(),
            ]
        );
    }

    #[test]
    fn escape_rolls_a_completion_back_even_without_a_popup() {
        // The reported defect: suggestions did not resolve this frame (the caller rebuilt
        // its candidate list), but a speculative completion still stands in the value.
        assert_eq!(
            escape_action(true, true, false),
            EscapeAction::RestoreTypedText
        );
        // Rollback wins over dismissing the list, whatever the popup state.
        assert_eq!(
            escape_action(true, true, true),
            EscapeAction::RestoreTypedText
        );
        // Nothing speculative: the usual dismiss ladder.
        assert_eq!(escape_action(true, false, true), EscapeAction::ClearHighlight);
        assert_eq!(escape_action(true, false, false), EscapeAction::ClosePopup);
        // Not pressed: never touch anything.
        assert_eq!(escape_action(false, true, true), EscapeAction::Ignore);
        assert_eq!(escape_action(false, false, false), EscapeAction::Ignore);
    }

    #[test]
    fn no_options_and_blank_queries_produce_nothing() {
        let empty: [&str; 0] = [];
        assert!(resolve_suggestions("Ин", 4, &empty, 8).is_none());
        assert!(resolve_suggestions("", 0, &["Ин Су Лин"], 8).is_none());
        assert!(resolve_suggestions("   ", 3, &["Ин Су Лин"], 8).is_none());
        assert!(resolve_suggestions("Ин", 4, &["Ин Су Лин"], 0).is_none());
    }
}
