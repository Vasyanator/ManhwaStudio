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
  `soft_hyphenate_overlong`) so it splits on the same orthographic boundaries as the
  renderer — keep-together particles, dictionary hyphenation points, and existing hard
  hyphens, with no emergency mid-word splits. Each block carries a `Joint` (how to join to
  the next block on the same line vs. when wrapped) with its break cost/word-break flag. It
  walks a break/no-break decision tree with shape pruning (a branch dies when a closed
  line breaks the shape), and accumulates a per-break "cost" (space 0, hard hyphen 1,
  dictionary hyphenation 2/3/4 by `classify_hyphen` quality) plus a word-break count for
  the panel's grouping and width/cost sorting. Before enumeration, `<no-break>`/`<nobr>`
  and machine `<m j>` inline ranges are stripped from the source text and their internal
  whitespace is converted to NBSP so the generated `formed_text` has no control tags but keeps
  those ranges as non-breaking blocks. Used by the typing panel's "Продвинутая
  форма текста" window and re-exported as `render_next::forms` so the renderer subsystem
  shares the same definitions.

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
- `TextWrapMode::None` must preserve caller text except for upstream normalization.
- `WholeWords` must avoid dictionary/emergency splitting. Minimal, Moderate, and
  Aggressive modes may use increasingly permissive split policy.
- Dictionary and emergency splits must respect safe text boundaries and must not split
  inside invalid UTF-8 or produce empty head/tail fragments.
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
