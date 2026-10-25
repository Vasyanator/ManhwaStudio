/*
File: crates/ms-text-render/src/wrap/forms/inline_tags.rs

Purpose:
Inline tags around the form search: tags are stripped for the search (no-break runs
glued into one block, the rest anchored), and put back on the applied form next to the
same words. Tag meaning comes only from `inline_styles::classify_inline_tag_body`.

Key functions:
- strip_inline_tags(), prepare_inline_no_break_text(), scopes_strip_alike()
- reapply_inline_tags_to_form_text(), place_tag_anchors(), classify_form_whitespace()
- `InlineTagScope` / `TagReapplyError` impls
*/

use crate::inline_styles::{InlineTagClass, classify_inline_tag_body};
use ms_text_util::segmentation::{NON_BREAKING_SPACE, SOFT_HYPHEN, is_line_end_dash_char};
use super::{
    FormWhitespaceStep, InlineNoBreakRun, InlineTagScope, StrippedFormText, TagAction, TagAnchor,
    TagReapplyError,
};

impl InlineTagScope {
    /// Font size the tag vocabulary is consulted with.
    ///
    /// `NoBreakOnly` acts only on the no-break and machine classes, and neither depends
    /// on the font size (they are decided before any percent value is resolved), so the
    /// value is arbitrary there — a style tag classified differently at `0.0` than at the
    /// real size is `Keep`ed either way.
    #[must_use]
    fn classify_font_size_px(self) -> f32 {
        match self {
            Self::NoBreakOnly => 0.0,
            Self::All { base_font_size_px } => base_font_size_px,
        }
    }

    /// What to do with a tag of `class` at this scope.
    ///
    /// `<no-break>` and `<br>` are CONSUMED rather than re-applied, deliberately: a form
    /// IS a complete line-break decision, so re-emitting the user's manual break would
    /// fight the form they just picked, and the protected range the no-break tag marks
    /// has already done its work by the time a form exists. `<br>` is consistent with a
    /// literal newline here, which the segmenter already treats as ordinary breaking
    /// whitespace (`ms-text-util`'s `is_breaking_whitespace`).
    ///
    /// Machine `<m …>` tags are re-applied at BOTH scopes: they are stripped at both, and
    /// a stripped tag that is never restored silently destroys the inline bold/colour/
    /// size/font/offset it carried — they are the panel's DEFAULT tag form.
    #[must_use]
    fn action_for(self, class: InlineTagClass) -> TagAction {
        match class {
            InlineTagClass::NoBreakOpen | InlineTagClass::NoBreakClose => TagAction::Consume,
            InlineTagClass::MachineOpen { .. } => TagAction::Reapply { closing: false },
            InlineTagClass::MachineClose => TagAction::Reapply { closing: true },
            InlineTagClass::Break => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Consume,
            },
            InlineTagClass::StyleOpen => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Reapply { closing: false },
            },
            InlineTagClass::StyleClose => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Reapply { closing: true },
            },
        }
    }
}

/// Removes inline tags from a RAW form source text and makes the whitespace
/// inside no-break ranges non-breaking (NBSP).
///
/// This is the character-level shape of what a form will contain, so it is what a width
/// metric has to measure ([`GlyphWidths::build`] calls it). It is deliberately PRIVATE:
/// the strip must happen exactly once per consumer, on the raw text. Handing an already
/// prepared string to the enumerators would leave them with no tags to find, every run
/// marked breakable, and the protected ranges hyphenated — the defect this privacy
/// prevents from coming back.
#[must_use]
pub(super) fn prepare_inline_no_break_text(text: &str, scope: InlineTagScope) -> String {
    strip_inline_tags(text, scope)
        .runs
        .into_iter()
        .map(|run| run.text)
        .collect()
}

/// The single strip pass: splits a RAW form source text into protected/unprotected runs
/// and records every tag it removed.
///
/// The vocabulary is not defined here — it comes from `inline_styles::classify_inline_tag_body`,
/// the same parser the renderer uses, so "what is a tag" cannot differ between the text the
/// form is built from and the text the renderer draws. `scope` decides only which of the
/// recognized classes are acted on ([`InlineTagScope::action_for`]).
#[must_use]
pub(super) fn strip_inline_tags(text: &str, scope: InlineTagScope) -> StrippedFormText {
    let classify_font_size_px = scope.classify_font_size_px();
    let mut runs = Vec::<InlineNoBreakRun>::new();
    let mut anchors = Vec::<TagAnchor>::new();
    let mut plain_len = 0usize;
    let mut no_break_depth = 0usize;
    let mut machine_stack = Vec::<bool>::new();
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(ch) = text[cursor..].chars().next() else {
            break;
        };
        if ch == '<'
            && let Some(rel_end) = text[cursor + ch.len_utf8()..].find('>')
        {
            let body_start = cursor + ch.len_utf8();
            let body_end = body_start + rel_end;
            let after_tag = body_end + '>'.len_utf8();
            if let Some(class) =
                classify_inline_tag_body(&text[body_start..body_end], classify_font_size_px)
            {
                let action = scope.action_for(class);
                if action != TagAction::Keep {
                    // The no-break bookkeeping belongs to the classes that are stripped at
                    // EVERY scope, so it can never be skipped by a `Keep`.
                    match class {
                        InlineTagClass::NoBreakOpen => {
                            no_break_depth = no_break_depth.saturating_add(1);
                        }
                        InlineTagClass::NoBreakClose => {
                            no_break_depth = no_break_depth.saturating_sub(1);
                        }
                        InlineTagClass::MachineOpen { protects_no_break } => {
                            if protects_no_break {
                                no_break_depth = no_break_depth.saturating_add(1);
                            }
                            machine_stack.push(protects_no_break);
                        }
                        InlineTagClass::MachineClose => {
                            if machine_stack.pop().unwrap_or(false) {
                                no_break_depth = no_break_depth.saturating_sub(1);
                            }
                        }
                        InlineTagClass::Break
                        | InlineTagClass::StyleOpen
                        | InlineTagClass::StyleClose => {}
                    }
                    if let TagAction::Reapply { closing } = action {
                        anchors.push(TagAnchor {
                            plain_offset: plain_len,
                            source: text[cursor..after_tag].to_string(),
                            closing,
                        });
                    }
                    cursor = after_tag;
                    continue;
                }
            }
        }

        plain_len += push_inline_no_break_text(
            &mut runs,
            &text[cursor..cursor + ch.len_utf8()],
            no_break_depth > 0,
        );
        cursor += ch.len_utf8();
    }

    StrippedFormText { runs, anchors }
}

/// Do two scopes strip `text` to exactly the same thing — same characters, same protected
/// runs, same tag anchors?
///
/// Everything the form engine derives from a (raw text, scope) pair goes through the one
/// strip pass: the break graph ([`search_forms`]), the width alphabet
/// ([`GlyphWidths::build`]) and the markup put back on the applied form
/// ([`reapply_inline_tags_to_form_text`]). Two scopes that strip a text alike are therefore
/// the SAME input for it, and a caller that caches a search by its input may treat them as
/// one — this is an exact statement about the engine, not an approximation of it.
///
/// That caller is the typing panel. [`InlineTagScope::All`] carries `base_font_size_px`
/// because the size decides whether `<offset=…>`/`<stretching=…>` are tags at all, so it
/// genuinely belongs to the input; but for a text whose tag bodies do not depend on it — any
/// text without such a tag, and any realistic size even with one — a font-size change cannot
/// move a single form, and restarting the search on it only throws the user's window filters
/// away.
#[must_use]
pub fn scopes_strip_alike(text: &str, a: InlineTagScope, b: InlineTagScope) -> bool {
    a == b || strip_inline_tags(text, a) == strip_inline_tags(text, b)
}

/// Appends one source character to the run list and returns how many BYTES it added to the
/// stripped text (an NBSP substitution makes that differ from the source character's size).
fn push_inline_no_break_text(
    runs: &mut Vec<InlineNoBreakRun>,
    text: &str,
    no_break: bool,
) -> usize {
    let prepared = if no_break {
        text.chars()
            .map(|ch| {
                if ch.is_whitespace() {
                    NON_BREAKING_SPACE
                } else {
                    ch
                }
            })
            .collect::<String>()
    } else {
        text.to_string()
    };
    if prepared.is_empty() {
        return 0;
    }
    let added = prepared.len();
    if let Some(last) = runs.last_mut()
        && last.no_break == no_break
    {
        last.text.push_str(prepared.as_str());
        return added;
    }
    runs.push(InlineNoBreakRun {
        text: prepared,
        no_break,
    });
    added
}

impl std::fmt::Display for TagReapplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unalignable {
                plain_offset,
                form_offset,
            } => write!(
                f,
                "form text diverges from its source at source byte {plain_offset} / form byte \
                 {form_offset}"
            ),
        }
    }
}

impl std::error::Error for TagReapplyError {}

/// Puts the inline tags of `source_text` back onto `form_text`, verbatim.
///
/// `form_text` must be [`TextForm::to_text`] of a form built from THIS `source_text` at
/// THIS `scope`; the walk aligns the two and inserts each removed tag before the character
/// it originally preceded. `<no-break>` and (at [`InlineTagScope::All`]) `<br>` are not
/// restored — see [`InlineTagScope::action_for`].
///
/// Placement rules, all of them contract:
/// - at a line break a CLOSING tag lands at the end of the preceding line (after a wrap
///   hyphen, so the hyphen stays inside the span it belongs to) and everything else at the
///   start of the following line;
/// - several tags at one source position keep their source order, except that a closing
///   and an opening tag at the same BREAK necessarily go to different lines;
/// - a tag inside a word that gets hyphenated stays attached to the character it preceded;
/// - a tag inside a protected range is an ordinary interior position;
/// - unclosed, nested and stray-closing tags are neither validated nor repaired: the output
///   is the user's own character sequence with line breaks interposed, so the renderer's
///   parser sees exactly what it sees for the un-formed text.
///
/// # Errors
/// [`TagReapplyError::Unalignable`] when the two texts cannot be aligned. The caller is
/// expected to fall back to the untagged `form_text` and say so — never to guess.
pub fn reapply_inline_tags_to_form_text(
    source_text: &str,
    scope: InlineTagScope,
    form_text: &str,
) -> Result<String, TagReapplyError> {
    let stripped = strip_inline_tags(source_text, scope);
    if stripped.anchors.is_empty() {
        return Ok(form_text.to_string());
    }
    let plain: String = stripped.runs.iter().map(|run| run.text.as_str()).collect();
    let mut placements = place_tag_anchors(plain.as_str(), form_text, &stripped.anchors)?;
    // Stable sort: anchors come out of the walk in source order, and only a break can send
    // a later anchor to an earlier offset (an ill-nested `<i></b>` split across two lines).
    placements.sort_by_key(|&(offset, _)| offset);

    let extra: usize = stripped
        .anchors
        .iter()
        .map(|anchor| anchor.source.len())
        .sum();
    let mut out = String::with_capacity(form_text.len() + extra);
    let mut copied = 0usize;
    for (offset, index) in placements {
        out.push_str(&form_text[copied..offset]);
        out.push_str(stripped.anchors[index].source.as_str());
        copied = offset;
    }
    out.push_str(&form_text[copied..]);
    Ok(out)
}

/// Breaking whitespace, exactly as the segmenter defines it
/// (`ms-text-util/src/segmentation/base.rs`, `is_breaking_whitespace`): everything
/// `char::is_whitespace` accepts except NBSP, which a protected range relies on.
#[must_use]
fn is_form_breaking_whitespace(ch: char) -> bool {
    ch.is_whitespace() && ch != NON_BREAKING_SPACE
}

/// End of the (possibly empty) run of breaking whitespace starting at `from`.
#[must_use]
fn breaking_whitespace_run_end(text: &str, from: usize) -> usize {
    let mut end = from;
    for ch in text[from..].chars() {
        if !is_form_breaking_whitespace(ch) {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

/// Is the form text about to close a line here, and where do the two sides of that break
/// begin?
///
/// Returns `(offset for a closing tag, offset for everything else)`. An optional wrap
/// hyphen is skipped first, so a closing tag lands AFTER it — the hyphen belongs to the
/// word the span covers.
#[must_use]
fn peek_form_line_break(
    plain: &str,
    src: usize,
    form_text: &str,
    tgt: usize,
) -> Option<(usize, usize)> {
    let mut at = tgt;
    if form_text[at..].starts_with('-') && is_form_wrap_hyphen(plain, src, form_text, at) {
        at += '-'.len_utf8();
    }
    form_text[at..]
        .starts_with('\n')
        .then(|| (at, at + '\n'.len_utf8()))
}

/// Is the `'-'` at `tgt` a hyphen the wrap inserted rather than one the source carries?
///
/// Unambiguous against a real hard hyphen, which the source has at the same position and
/// which therefore matches before this is ever consulted.
///
/// A wrap hyphen is always followed by the `'\n'` that separates it from the rest of the
/// source: `build_line_text_and_units` appends `Joint::wrap_suffix` only to a line that
/// WRAPS (`ms-text-util/src/segmentation/base.rs`), so the last line of a form never ends
/// on one. End of text is therefore NOT a legal context — accepting it let an alleged form
/// append a visible character to the text and still be re-tagged.
#[must_use]
fn is_form_wrap_hyphen(plain: &str, src: usize, form_text: &str, tgt: usize) -> bool {
    !plain[src..].starts_with('-') && form_text[tgt + '-'.len_utf8()..].starts_with('\n')
}

/// Classifies the whitespace boundary at (`src`, `tgt`), or `None` when no form of this
/// source could have produced it.
///
/// `src_run_end` / `tgt_run_end` are the ends of the (possibly empty) breaking-whitespace
/// runs starting at the two cursors.
#[must_use]
fn classify_form_whitespace(
    plain: &str,
    src: usize,
    src_run_end: usize,
    form_text: &str,
    tgt: usize,
    tgt_run_end: usize,
) -> Option<FormWhitespaceStep> {
    let src_run = &plain[src..src_run_end];
    let tgt_run = &form_text[tgt..tgt_run_end];
    if !src_run.is_empty()
        && src_run.chars().count() == tgt_run.chars().count()
        && let (Some(src_char), Some(tgt_char)) = (src_run.chars().next(), tgt_run.chars().next())
    {
        return Some(FormWhitespaceStep::Normalised { src_char, tgt_char });
    }
    if tgt_run == "\n" {
        if !src_run.is_empty() {
            return Some(FormWhitespaceStep::CollapsedIntoBreak);
        }
        return form_text[..tgt]
            .chars()
            .next_back()
            .is_some_and(is_line_end_dash_char)
            .then_some(FormWhitespaceStep::BreakAfterDash);
    }
    let at_text_start = src == 0 && tgt == 0;
    let at_text_end = src_run_end == plain.len() && tgt == form_text.len();
    (tgt_run.is_empty() && !src_run.is_empty() && (at_text_start || at_text_end))
        .then_some(FormWhitespaceStep::TrimmedTextEdge)
}

/// Aligns the stripped source with the form text and returns `(form byte offset, anchor
/// index)` for every anchor, in anchor order.
///
/// Two monotone cursors, one target character consumed per step; the walk never searches,
/// so it cannot lock onto a later occurrence of a repeated word. Every step is one of: a
/// literal match, a whitespace boundary the form engine can actually produce
/// ([`FormWhitespaceStep`] — that enum, not this list, is the authority), a wrap hyphen the
/// form added ([`is_form_wrap_hyphen`]), or a soft hyphen the segmenter consumed. Anything
/// else is [`TagReapplyError::Unalignable`].
fn place_tag_anchors(
    plain: &str,
    form_text: &str,
    anchors: &[TagAnchor],
) -> Result<Vec<(usize, usize)>, TagReapplyError> {
    let mut placements = Vec::with_capacity(anchors.len());
    let mut src = 0usize;
    let mut tgt = 0usize;
    let mut next = 0usize;

    loop {
        // Anchors the source cursor has reached. `<=` and not `==`: a collapsing whitespace
        // run moves the cursor by more than one character, and its interior anchors are
        // placed by that step itself.
        while next < anchors.len() && anchors[next].plain_offset <= src {
            let offset = match peek_form_line_break(plain, src, form_text, tgt) {
                Some((close_at, open_at)) => {
                    if anchors[next].closing {
                        close_at
                    } else {
                        open_at
                    }
                }
                None => tgt,
            };
            placements.push((offset, next));
            next += 1;
        }
        if src >= plain.len() && tgt >= form_text.len() {
            break;
        }

        let src_ch = plain[src..].chars().next();
        let tgt_ch = form_text[tgt..].chars().next();

        // A soft hyphen the user typed: the segmenter cuts at one and drops it, so it is in
        // the source and never in the form.
        if src_ch == Some(SOFT_HYPHEN) && tgt_ch != Some(SOFT_HYPHEN) {
            src += SOFT_HYPHEN.len_utf8();
            continue;
        }

        if src_ch.is_some_and(is_form_breaking_whitespace)
            || tgt_ch.is_some_and(is_form_breaking_whitespace)
        {
            let src_run_end = breaking_whitespace_run_end(plain, src);
            let tgt_run_end = breaking_whitespace_run_end(form_text, tgt);
            let step =
                classify_form_whitespace(plain, src, src_run_end, form_text, tgt, tgt_run_end)
                    .ok_or(TagReapplyError::Unalignable {
                        plain_offset: src,
                        form_offset: tgt,
                    })?;
            match step {
                // Consume one pair at a time so interior anchors are still seen by the
                // loop head.
                FormWhitespaceStep::Normalised { src_char, tgt_char } => {
                    src += src_char.len_utf8();
                    tgt += tgt_char.len_utf8();
                }
                // The whole run is replaced at once — by a break, or by nothing at a
                // trimmed edge. Its interior anchors take the two sides of the break.
                FormWhitespaceStep::CollapsedIntoBreak
                | FormWhitespaceStep::BreakAfterDash
                | FormWhitespaceStep::TrimmedTextEdge => {
                    let close_at = tgt;
                    let open_at = tgt_run_end;
                    while next < anchors.len() && anchors[next].plain_offset < src_run_end {
                        let offset = if anchors[next].closing {
                            close_at
                        } else {
                            open_at
                        };
                        placements.push((offset, next));
                        next += 1;
                    }
                    src = src_run_end;
                    tgt = tgt_run_end;
                }
            }
            continue;
        }

        if let (Some(s), Some(t)) = (src_ch, tgt_ch)
            && s == t
        {
            src += s.len_utf8();
            tgt += t.len_utf8();
            continue;
        }

        if tgt_ch == Some('-') && is_form_wrap_hyphen(plain, src, form_text, tgt) {
            tgt += '-'.len_utf8();
            continue;
        }

        return Err(TagReapplyError::Unalignable {
            plain_offset: src,
            form_offset: tgt,
        });
    }

    Ok(placements)
}
