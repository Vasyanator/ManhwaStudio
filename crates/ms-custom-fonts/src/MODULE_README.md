# Module: crates/ms-custom-fonts/src (crate `ms-custom-fonts`)

## Purpose
The single owner of "what a custom font recipe is and how it compiles into a TrueType face".
A custom font is DEFINED by its recipe only (references to real fonts by identity plus the
kind's parameters); the compiled face is a disposable in-memory derivative that callers cache
by `RecipeKey` and regenerate whenever it is missing. Kinds: `Hybrid` (weighted outline
average of 2..=4 fonts, per-glyph path chosen by one of five `BlendAlgorithm`s: `Auto`, `Sdf`,
`NoBlend`, `Skeleton`, `Transfer`) is implemented; `Combined` (per-character source choice) is planned and
arrives as a new `CustomFontKind` variant with its own compiler module beside `hybrid/`.

## Architecture
Level of `ms-text-render` (below `ms-tab-typing`); depends on no `ms-*` crate. GUI-free, no
i18n, no file or network I/O, no threads of its own, no logging: bytes in, bytes out, typed
errors that carry their context (the caller logs and localizes).

```text
recipe (CustomFontRecipe) + SourceFont[] (bytes, recipe order)
  -> source.rs   parse + normalize to 1000 units/em (cmap, outlines, metrics, effective kerning)
  -> hybrid/     plan characters -> blend glyphs (rayon) -> blend metrics + kerning
  -> sfnt.rs     write the TrueType file
  -> CompiledFont { bytes, family/PostScript name, key, coverage report, BlendStats }

export only: CompiledFont.bytes + the font's OWN user pairs
  -> kern_override.rs (with_kerning_overrides) -> sfnt::replace_kern -> new bytes, same names
```

## Files and submodules
- `lib.rs`: public surface — `compile`, `coverage_report`, `included_chars` (which of the
  given characters the face maps: the compiler's own per-character decision, for callers that
  classify coverage without compiling), `CompileScope`, `CompiledFont`, `BlendStats` (glyphs
  per outline path of one compile), `CoverageReport`, `ENGINE_VERSION`, re-exports; dispatch
  by kind.
- `recipe.rs`: recipe model (incl. the hybrid's `BlendAlgorithm`, `PropertyWeights` and the
  weights owner `HybridParams::effective_weights`), canonical JSON (`from_json` / `to_json`),
  `validate`, `RecipeError` (`UnknownKind` / `UnknownAlgorithm` distinct from `Malformed`).
- `key.rs`: `SourceDigest` / `source_digest`, `RecipeKey` / `recipe_key`, preview subset tag,
  the compiled-face naming rule and `RecipeKey::verify_full_face` (`VerifiedFullFace`,
  `FaceKeyMismatch`).
- `error.rs`: `CompileError`.
- `source.rs`: `SourceFont` (caller input) and `SourceFace` (parsed, normalized view).
- `kerning.rs`: `KerningPairPerMille` and the uncapped built-in pair reader (GPOS + `kern`).
- `outline.rs`: outline model, `ttf-parser` collector, flattening, quantization to TrueType
  quadratic contours (`TARGET_UPM` = 1000).
- `sfnt.rs`: the hand-written static `glyf` font writer, and `replace_kern` (re-assembles a
  face it wrote with another `kern` table).
- `kern_override.rs`: `with_kerning_overrides` — a compiled face with a font's OWN user pairs
  baked into `kern` (for files handed to other programs, e.g. the PSD bundle).
- `hybrid/`: the hybrid compiler; see `hybrid/MODULE_README.md`.

## Contracts and invariants
- Recipe JSON, baseline (algorithm `Auto`, no property weights):
  `{"kind":"hybrid","sources":[{"identity","weight","missing":null|"disable"|"no_influence"}],
  "missing_default"}` — byte-identical to what builds before algorithms wrote. Every other
  hybrid: `{"kind":"hybrid2", ..same.., "algorithm":"auto"|"sdf"|"no_blend"|"skeleton"|
  "transfer", "base_source":N (transfer only)}` with `"shape"`/`"spacing"`/`"kerning"` on every
  source or on none. Unknown extra fields are ignored on read; an unknown `kind` is
  `RecipeError::UnknownKind` and an unknown `hybrid2` algorithm `RecipeError::UnknownAlgorithm`
  (checked before any other `hybrid2` field), so stores keep such recipes verbatim.
- **The kind tag is the compatibility gate.** Builds before `hybrid2` ignore unknown fields
  under `hybrid` but keep an unknown kind verbatim and unavailable. So any recipe parameter
  that changes output away from the plain weighted average MUST force the `hybrid2` tag
  (`HybridParams::is_baseline` decides; `to_json` and `CustomFontKind::tag` follow it), and a
  `hybrid2` this build cannot read is unsupported everywhere, never compiled as `Auto`. A new
  parameter of that kind needs a new tag, not a new field.
- Weights: `HybridParams::effective_weights` is the one owner of which weight applies where —
  shape weights shape outlines, spacing weights advances and vertical metrics, kerning weights
  inherited pairs; without property weights all three are the source's `weight`.
- `validate`: 2..=4 sources, finite weight > 0, non-empty identity, no duplicate identity under
  the CALLER's normalization (passed in), property weights one per source and finite > 0,
  transfer base index in range; the first violation in a documented order wins. `compile`
  re-checks with exact identities only.
- Sources are passed in recipe order with identical identity strings (`SourceMissing` /
  `SourceMismatch` otherwise).
- `RecipeKey` = SHA-256 over a fixed binary encoding of `ENGINE_VERSION`, the recipe (kind tag,
  identity, weight bits, overrides, default policy, in order; for `hybrid2` then per source a
  property-weight flag + bits, the algorithm byte and the transfer base) and, per source, its
  content digest and EFFECTIVE user kerning (sorted). A baseline recipe's encoding is the
  pre-algorithm one, so its key is unchanged (pinned by a test vector, as is a `hybrid2` key). Stable across machines and Rust releases (never
  `DefaultHasher`). The hybrid's OWN user kerning is not in the binary nor in the key.
- **ENGINE_VERSION bump rule:** any change that can alter output bytes for the same inputs —
  blend or alignment math, grid / fit / quantization constants, writer layout, naming, kerning
  or missing-glyph rules — bumps `ENGINE_VERSION` in the same change, or stale caches survive.
- Names: full face `MS Custom <short8>` / `MSCustom-<short8>`, unique id (`name` ID 3)
  `<PostScript name>;<key hex>;ms-custom-fonts engine <N>` (the WHOLE key); a stored face is
  trusted for a key only through `RecipeKey::verify_full_face` (PostScript name AND unique id,
  so two keys sharing the 32-bit prefix never accept each other's face); subset (preview) face
  `MS Custom Preview <short8> <subset8>` / `MSCustomPreview-<short8>-<subset8>`, so a preview
  never shares (family, weight, style) with the full face in the renderer's pool and never
  reuses a bundled family name (`font_registry` taint/displace rules).
- Output tables: `OS/2 cmap(4+12) glyf head hhea hmtx kern loca maxp name post(v3)`. DROPPED in
  v1: GSUB (ligatures, `locl`, `vert`), GPOS (mark positioning; pair kerning ships as `kern`,
  which rustybuzz applies when no GPOS exists), hinting, colour/bitmap glyphs, variations,
  vertical metrics. Callers must disclose this to the user.
- Determinism: output bytes are a pure function of the inputs on a given build and platform,
  independent of the rayon thread count (per-glyph independent work assembled by index; no
  timestamps). Across platforms the libm `hypot` used inside distance computations may differ
  in the last bit, which can rarely flip one rounded coordinate; caches are keyed by the
  recipe, never by output bytes, so this only means two machines may hold slightly different
  cached faces.
- Own user kerning (`with_kerning_overrides`): the renderer's REPLACE-not-add rule in file form
  — an override replaces the face's value for its glyph pair, `0.0` removes it, unmapped
  characters are ignored, a repeated pair's first occurrence wins, a glyph pair shared by
  several character pairs is decided by the smallest one (as the inherited kerning does).
  Values are rounded to font units (1/1000 em), so a sub-unit override is the file's only
  deviation from the renderer. Names and every other table stay byte-identical; empty
  overrides reproduce the input. It never feeds a cache or the `RecipeKey`: the cached face
  keeps inherited kerning only.
- No panics on bad font data: unreadable sources, out-of-range coordinates and format limits
  are `CompileError`s. A glyph whose outline exceeds the blend work bounds (point budget,
  extent, non-finite coordinates) is `CompileError::GlyphTooComplex` and fails the WHOLE
  compile; a glyph is never dropped silently (bounds: `hybrid/MODULE_README.md`).

## Editing map
- New custom font kind: a `CustomFontKind` variant + JSON in `recipe.rs`, its key encoding in
  `key.rs`, a compiler directory beside `hybrid/` that fills a `sfnt::FontBuild`, and the
  dispatch arms in `lib.rs`. Bump `ENGINE_VERSION` only if existing kinds' bytes change.
- Output table layout / new table: `sfnt.rs` (+ `ENGINE_VERSION`).
- How own pairs are baked into an exported face: `kern_override.rs` (table encoding stays in
  `sfnt.rs`).
- How sources are read (cmap, outlines, metrics): `source.rs`, `outline.rs`.
- Which built-in kerning counts: `kerning.rs`.
- Blend algorithm, coverage rules, inherited kerning: `hybrid/`.
- New hybrid algorithm: a `BlendAlgorithm` variant (JSON spelling, key byte in `key.rs`), its
  arm in `hybrid::blend_glyph` and a `BlendStats` count; it is non-baseline by construction.
