# Module: src/tabs/typing/render_next/wrap

## Purpose
This directory owns text wrapping before rasterization for the production typing
renderer. It prepares layout text for horizontal, shape-aware, and vertical modes while
keeping dictionary hyphenation and emergency split rules out of glyph drawing code.

## Architecture
`mod.rs` exposes the small internal wrap boundary used by `pipeline.rs`:
`reshape_text_for_shape`, `build_vertical_layout_text`, hyphenation dictionaries, and
wrap-mode policy helpers.

The main data flow is:

1. `pipeline.rs` normalizes text, resolves inline styles, font metrics, and wrap mode.
2. `mod.rs` maps `TextWrapMode` to a `WordBreakPolicy` and dictionary requirements.
3. `horizontal.rs` scores candidate line breaks for free/rectangle/shape targets.
4. `hyphenation.rs` supplies safe dictionary and emergency split points.
5. `shape.rs` builds rectangle/oval/hexagon width profiles and iteratively rewraps
   horizontal lines.
6. `vertical.rs` prepares newline-separated vertical columns for the vertical raster
   path.

This module returns layout text and warnings only. It does not draw glyphs, allocate
output images, or apply effects.

## Files and submodules
- `mod.rs`: internal public surface, wrap-mode policy mapping, shared constants, and
  hanging-punctuation classification. The hanging set itself is not hardcoded here:
  `is_hanging_punctuation` delegates to the app-wide editable list in
  `crate::text_punctuation` (default in `TextTab.hanging_punctuation`, edited in
  Settings → General).
- `horizontal.rs`: DP/scored paragraph wrapping, line-width measurement, candidate
  break collection, keep-together heuristics, and target-width scoring.
- `hyphenation.rs`: embedded Russian/English dictionaries, soft-hyphen insertion,
  safe split filtering, dictionary split lookup, and emergency split fallback.
- `shape.rs`: shape width profiles for rectangle/oval/hexagon, soft peak no-tree ordering,
  iterative horizontal reshaping, and approximate-shape warnings.
- `vertical.rs`: vertical column preparation, paragraph splitting, shape-aware vertical
  targets, and vertical emergency token splitting.
- `forms.rs`: shared discrete line-break "form" logic (presets `FreeNoTree`/`Lens`/
  `Widen`/`Narrow`, pluggable `LineWidthMetric` line widths — `GlyphWidths` measures
  pixel widths via cosmic-text shaping with a precomputed per-glyph advance + adjacent-pair
  kerning table, `CharWidthMetric` is the no-font fallback; both honor the hanging-punctuation
  edge rule — tolerance-aware form predicates, single-pass deduplicated `enumerate_forms`,
  the ranked `search_forms` (see "Form search" below), and `choose_form`). The
  enumerator reuses the shared text segmenter (`segmentation::Segmenter::segment` after
  the dictionary soft-hyphen markup of `prepare_form_text`) so it splits on the same
  orthographic boundaries as the renderer — keep-together particles, dictionary
  hyphenation points, and existing hard
  hyphens, with no emergency mid-word splits. Each block carries a `Joint` (how to join to
  the next block on the same line vs. when wrapped) with its break cost/word-break flag. It
  walks a break/no-break decision tree with shape pruning (a branch dies when a closed
  line breaks the shape), and accumulates a per-break "cost" (space 0, hard hyphen 1,
  dictionary hyphenation 2/3/4 by `classify_hyphen` quality) plus a word-break count for
  the panel's grouping and width/cost sorting. Used by the typing panel's "Продвинутая
  форма текста" window and re-exported as `render_next::forms` so the renderer subsystem
  shares the same definitions.

## Inline tags: the text arrives RAW and the tags go back

`enumerate_forms`, `search_forms`, `choose_form` and `GlyphWidths::build` all take the
**raw** source text — every inline tag included — plus an `InlineTagScope`. `forms.rs` is
the ONLY place that removes them (`strip_inline_tags`, private for exactly that reason) and
the ONLY place that puts them back (`reapply_inline_tags_to_form_text`). It strips once per
consumer, on the raw string. Handing any of them an already stripped text is the defect
this rule exists to prevent: the tags are gone, every run looks breakable, and the range the
user protected gets dictionary hyphenation. A caller must therefore never "prepare" the text
first.

**The vocabulary is not defined here.** `inline_styles::classify_inline_tag_body` — a thin
wrapper over the same `parse_inline_tag` the renderer's `parse_inline_style_tags` uses — is
the single authority on what a `<…>` body means. Recognition is value-dependent
(`<size=abc>` is not a tag) and, for `<offset=…>`/`<stretching=…>`, font-size dependent,
which is why `InlineTagScope::All` carries `base_font_size_px`. Two dispatch tables would
mean the form is built from a different text than the one drawn, and every restored tag
would land next to the wrong word.

**`InlineTagScope` is a correctness switch, not a preference.** With «Инлайновые теги» OFF
the renderer does not parse tags at all, so `<b>` is literal text the user wants DRAWN and
measured: `NoBreakOnly` keeps it. With the flag ON the renderer consumes it, so `All` removes
it. The no-break vocabulary and machine `<m …>` are stripped at BOTH scopes. **The search
and its width metric must be given the same scope** — a mismatch measures a different
alphabet than it segments; the typing panel derives it once
(`create_advanced::advanced_form_inline_tag_scope`) and hands the same value to both.

**What comes back, and what does not.** Style tags and machine `<m …>` are re-emitted
VERBATIM: re-serializing from a parsed style model would rewrite `<b>` as `<m b=1>`,
normalize the user's spelling and have to invent a nesting for stray tags. `<no-break>` and
`<br>` are CONSUMED and never restored — a form IS a complete line-break decision, so
re-emitting the user's manual break would fight the form just chosen, and the protected
range has already done its work by the time a form exists. Only the APPLIED form is
re-tagged; the enumerated ones (and the panel's preview cards) stay tag-free.

**Two scopes that strip a text alike are the same input** (`scopes_strip_alike`, public for
exactly one reason). Everything the engine derives from a (raw text, scope) pair goes through
the strip, so a caller that caches a search by its input may treat such scopes as one. The
typing panel does: `InlineTagScope::All` carries `base_font_size_px` because the size decides
whether `<offset=…>`/`<stretching=…>` are tags at all, and without this a mere font-size change
would restart the search and wipe the window's display filters for a text where no tag body
depends on it.

**Anchors are byte offsets into the PRODUCED stripped text**, recorded as its length at the
moment the tag was met — so the NBSP widening (a 1-byte space becomes a 2-byte NBSP) is
already accounted for and cannot shift them.

**Placement is a two-cursor forward walk** over (stripped text, form text) with no searching,
so it cannot lock onto a later occurrence of a repeated word. Its steps are a literal match, a
whitespace boundary, a wrap hyphen the form added, and a soft hyphen the segmenter consumed.

The whitespace boundaries it accepts are exactly the four `ms-text-util` `Joint` kinds can
produce, enumerated as `FormWhitespaceStep` next to the walk — equal-count normalisation
(`Joint::space` carries `" ".repeat(n)`), a whole run replaced by the single `'\n'` of a break,
a `'\n'` inserted where the source has no whitespace (only ever right after a dash, because
that is the one junction without a separator that can break), and the leading/trailing
whitespace of the WHOLE text (`preserve_edge_spaces: false`; there is no interior trim). A wrap
hyphen likewise counts as one only when the `'\n'` that separates it from the rest of the
source follows it — the last line of a form never wraps, so a trailing `'-'` is not one.
**Refusal is the point of that list**: anything outside it means the form text is not this
source's, and the walk **refuses** (`TagReapplyError::Unalignable`) instead of resynchronizing —
a silently misplaced `<font=…>` restyles the wrong words, and a form that invents or swallows a
space would change what the reader sees. The caller falls back to the untagged form text, logs,
and tells the user. Both directions are pinned by tests: the illegal cases must be refused, and
`no_form_the_engine_actually_produces_is_refused` walks every form of a corpus to prove the
legal ones are not.

At a break a CLOSING tag lands at the end of the preceding line — after the wrap hyphen, so the
hyphen stays inside the span it belongs to — and everything else at the start of the following
one.

A protected range is ONE unbreakable block, against all three break kinds:

- **space** — turned into NBSP, which the segmenter does not break on;
- **dictionary hyphenation** — dictionary soft hyphens are applied to the unprotected
  runs only, so no soft hyphen is ever placed inside a protected range. Marking the
  runs up one by one is safe because every hyphenation rule is local to a word
  (`ms-text-util`'s segmentation README): a run hyphenates exactly as it would inside
  the whole text, so a no-break tag never changes how the text OUTSIDE it breaks;
- **existing hard hyphen** — the segmenter always cuts at one (`allow_hard_hyphen_breaks:
  true`, which the rest of the text needs), so `segment_form_blocks` records the protected
  byte ranges of the text it feeds the segmenter and afterwards merges every junction that
  falls strictly inside one (`glue_protected_junctions`). Locating the junctions relies on
  a documented property of `ms-text-util`'s segmenter (`block_spans`): block texts are
  ordered, non-overlapping literal substrings of its input. If that ever stops holding, the
  mapping is refused and logged, and only the hard-hyphen protection degrades.

A junction exactly ON a range boundary is not glued — the protected text stays whole there.

## Form search (`forms.rs::search_forms`)

`forms.rs` has two entry points into the break tree and they answer different
questions. `enumerate_forms` is the original exhaustive walk: every form that
matches the preset, in tree order, bounded only by `max_forms`, free memory and a
node ceiling. It is what `choose_form` (the renderer's single-pick path) uses, and
on a p99 text it is both too slow and meaningless — the set it returns is the DFS
prefix, not a sample of good forms.

`search_forms` is the ranked search designed in `dev-docs/text_forms_ranking_plan.md`.
Three layers, deliberately kept separate — admissibility decides what may exist,
quality decides what is good, ordering decides what is shown first:

- **Layer A — admissibility, inside the search.** Aspect cap
  `max_width / (lines × line_height_units) <= aspect_max` (default 21:9); the width
  **corridor**; the hyphen budget; the preset predicate; and the optional hard
  `line_range` / `width_range`. All of them *prune*: a line count outside
  `line_range` is never walked and `width_range`'s upper bound clamps the corridor.
  Every prune must be **admissible** — it may never discard a form the final
  acceptance test would admit. Two consequences that look like missing
  optimisations and are not: there is no per-bucket pre-skip on the ideal width
  `T_L` (`T_L` is not a lower bound on a form's max width — a break eats the
  inter-word space), and the corridor's upper bound stops the scan only past a
  proven slop (see the `LineWidthMetric` contract below), because line width is
  not monotone in the break index.
- **Layer B — quality `Q`** (`TextForm::quality_milli`, ×1000, **lower is better**):
  roughness, unevenness, hyphen-budget fill, mean break cost, short head, short
  tail, hyphen runs. Deliberately width-agnostic and normalised by the form's own
  median, so `Q` is comparable across line counts.
- **Layer C — order.** `search_forms` only *groups*: forms come out bucketed by
  line count ascending, each bucket sorted by `quality_milli` ascending and cut to
  `per_bucket`. Round-robin emission, narrow lean and the quality floor belong to
  the caller (the typing panel).

Why the corridor exists: exhaustive enumeration is not merely slow at p99, it is
useless — the mass of forms sits at middle line counts and differs by shuffling
one short line. Searching per line count `L` inside `[interior_lo, interior_hi] × T_L`
(where `T_L = total_single_line_width / L`) is at once the performance fix and the
formal statement of "smooth form without abrupt width jumps". A bucket that comes
out EMPTY — and only such a bucket — is retried down a relaxation ladder, so a rich
bucket is never polluted by a rung a different height needed.

Why the hyphen relaxation keys on slack, not on aspect: for a small text "vertical"
and "narrow" coincide, for a large one they do not — 24 lines × 13 chars is very
tall yet has plenty of room to avoid hyphens. Keying on the aspect would hand a
large text a free pass to hyphenate most of its lines. `slack = max_width /
min_possible_width` (the widest single block measured as a wrapping line, hyphen
included) relaxes exactly the forms where hyphenation is unavoidable, at any text
size.

Contracts:

- every numeric decision of the plan is a field of `FormSearchParams` (or of the
  nested `CorridorLevel` / `HyphenBudget` / `QualityWeights`); the algorithm body
  carries no tuning constants;
- `search_forms` **sanitises** those params first (`FormSearchParams::sanitized`):
  a non-finite or out-of-domain real is silently replaced by its default. Without
  it a single `NaN` voids a hard guarantee, because every comparison against `NaN`
  is false (`NaN` aspect cap = no cap, `NaN` hyphen ratio = no budget). The crate is
  GUI-free and runs per text image, so the replacement is silent, deterministic and
  unlogged;
- `LineWidthMetric` implementations owe the search one property (documented on the
  trait): appending a block to a line may not shrink its width by more than
  "widest single block" + `line_width("-")`. Full monotonicity is *not* assumed —
  the wrap hyphen already breaks it;
- `line_height_units` is supplied by the caller in the units of the active metric —
  only the caller knows the px→units conversion (see the field's doc comment);
- no form wider than `aspect_max` is returned, EXCEPT after the mandatory
  empty-result fallback: when the cap admits nothing (one long unbreakable word),
  the search is re-run once with the cap lifted so the window is never empty;
- `node_budget_total` is the ceiling of the **whole call**: both runs share one
  `SearchContext`, so the fallback spends what the first run left. If the first
  run exhausted it, the fallback does not run at all and the result is `truncated`
  — an exhausted hard budget outranks "the window is never empty";
- each form is emitted exactly once **by construction** (a root-to-leaf path *is*
  the cut vector, the ladder only retries an empty bucket), not by a dedup set: a
  64-bit-hash dedup silently dropped a valid form on a collision;
- the line memo (`WidthMemo`) stores widths only — never line text — and switches
  from a dense `n²` table to a capped hash map above `DENSE_WIDTH_MEMO_MAX_CELLS`,
  so a pasted 100 000-character text cannot allocate gigabytes before any budget
  is consulted;
- `truncated` means a budget was hit (nodes, per-bucket form cap, memory), never
  the `per_bucket` curation; `nodes_visited` reports the work done;
- bounded by node counts only. There is **no wall-clock deadline**: this crate
  builds for wasm, where `std::time::Instant` is not universally safe, and node
  budgets are deterministic and testable;
- `TextForm` derives `Eq`, so every derived real is stored as an integer
  (`quality_milli`, `roughness_pct`, `aspect_milli`, `unevenness_pct`) — never add
  an `f32` field to it. `line_widths` is filled by both entry points so consumers
  never re-measure; `quality_milli`/`aspect_milli` are filled only by
  `search_forms` (`enumerate_forms` leaves `UNSCORED_QUALITY_MILLI` / `0`).

## Contracts and invariants
- Wrapping uses normalized text from `pipeline.rs`; inline style byte-offset remapping
  must happen outside or around this module, not by applying original tagged spans here.
  `forms.rs` is the documented exception and only for text→text work: it strips tags from a
  raw form source and re-emits them verbatim onto the chosen form, never mapping style spans.
- `TextWrapMode::None` must preserve caller text except for upstream normalization.
- `WholeWords` must avoid dictionary/emergency splitting. Minimal, Moderate, and
  Aggressive modes may use increasingly permissive split policy.
- Dictionary and emergency splits must respect safe text boundaries and must not split
  inside invalid UTF-8 or produce empty head/tail fragments.
- Neither split looks at letter case: an all-caps word is hyphenated and
  emergency-split exactly like its lowercase form. `hyphenation.rs::find_emergency_split_index`
  therefore takes the block alone, with no whole-text context to thread down.
- Shape wrapping returns warnings when it uses approximate fallback behavior; do not
  hide those warnings.
- `TextShape::Rectangle`, `Oval`, and `Hexagon` profiles must keep line widths positive
  and respect `shape_min_width_percent`; `SoftPeak` ignores that minimum-width slider and uses
  `shape_variant` to bias among valid no-tree layouts while preserving nondecreasing line units up
  to the middle and nonincreasing units after it.
- Vertical wrapping prepares columns only; glyph positioning and optical spacing belong
  in `../layout/vertical.rs`.
- Measurement through `cosmic-text` is allowed for scoring, but this module must not
  rasterize glyphs or mutate output images.

## Editing map
- To change wrap-mode semantics or shared constants, edit `mod.rs` and update focused
  tests for mode mapping.
- To change horizontal line scoring, candidate generation, keep-together rules, or
  target-width balancing, edit `horizontal.rs`.
- To change language dictionaries, safe split rules, soft hyphenation, or emergency
  split behavior, edit `hyphenation.rs`.
- To change rectangle/oval/hexagon shaping or shape fallback warnings, edit `shape.rs`.
- To change vertical column preparation, edit `vertical.rs`; edit `../layout/vertical.rs`
  only for glyph placement after wrapping.
- To retune the ranked form search (corridor tightness, aspect cap, hyphen
  relaxation, quality weights, budgets), change the defaults in
  `forms.rs::FormSearchParams::default()` and the associated `DEFAULT` consts — not
  the algorithm bodies. To change *what is shown first*, edit the panel: the crate
  only groups and sorts within a bucket.
