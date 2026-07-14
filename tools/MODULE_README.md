# Module: tools

## Purpose
Offline developer/agent utilities that operate on the repository but are **not**
part of the shipped runtime. Nothing here is imported by `src/` or the `crates/`;
these scripts are run by hand during migrations and audits.

Current contents relate to the i18n migration: routing the ~4,800 hardcoded
Russian UI string literals through the `ms-i18n` `t!` / `tf!` macros.

## Architecture
Pure Python 3 (stdlib only — no third-party deps). The i18n tool is a single-file
pipeline. Migration is **two-step**: the tool finds and rewrites deterministically;
a human supplies the semantic key names. The tool never invents a final key.

```
find_rust_files -> lex_rust (strings + bracket-safe masked source)
               -> classify_literal (per-literal Action)
               -> KeyRegistry.allocate (deterministic *suggested* keys only)
               -> build_report                      (--dry-run)
               -> build_proposal / write_proposal   (--propose -o FILE)
               -> load_key_mapping + assign_keys_from_mapping
                  + apply_to_file + merge_catalog_file  (--apply --keys FILE)

compute_untranslated(<tag>.json, ru.json)            (--untranslated <tag>)
```

The lexer never uses a Rust parser: it tokenises just enough to find string
literals (raw / escaped / multi-line) and to produce a *masked* copy of the source
where comment and string content is blanked, so bracket matching and call-context
detection cannot be fooled by punctuation inside strings or comments.

### Two-step migration contract
1. `--propose <path> -o proposal.json` — classify (identical Action logic and
   exclusions as the dry run) and emit one entry per migratable hit:
   `{file, line, action, text, context_fn, suggested_key, format_args?}`.
   `suggested_key` is a transliterated **starting point only**; writes no `.rs`.
2. A human replaces each entry's `suggested_key` with a semantic `key` field.
3. `--apply <path> --keys proposal.json` — rewrite call sites using **exactly** the
   supplied `key` per hit (matched by the `(file, line, text)` call site, stable
   across step 1→3 with no source edits), then emit/merge the catalogs. A migratable
   hit with no mapping entry, or an entry with no non-empty `key`, **aborts the run**
   (exit 2, nothing written) naming its `file:line`. This is the guard against a
   half-named migration; the tool never falls back to an invented key.

### Key naming convention (human-supplied)
- Shape: `<area>.<screen_or_module>.<meaning>` — e.g. `settings.canvas_ribbon.preset_label`,
  `settings.typesetting.hanging_punctuation_hint`, `widgets.spellcheck.add_to_dictionary`.
- Do **not** encode the enclosing `fn` name (an implementation detail that churns).
- Do **not** encode the Russian text (a copy-edit would rename the key and orphan the
  es/fr/pt translations).
- Lowercase `snake_case` segments, dot-separated, ASCII only.
- Name by **meaning and role** (`_label`, `_hint`, `_button`, `_title`, `_error`,
  `_tooltip`), not by the sentence. Identical labels reused in one role may share a key.
- The transliterated key scheme (`module.fn.slug`) survives only as `suggested_key`.

## Files and submodules
- `i18n_extract.py`: the extraction tool. Modes: `--dry-run` (default, writes
  nothing except an optional `--report` under `docs/`), `--propose <path> -o FILE`
  (emit a key proposal, no `.rs` writes), `--apply <path> --keys FILE` (rewrite
  `.rs` sources with the human-supplied keys and merge `crates/ms-i18n/locales/*.json`),
  and `--untranslated <tag>` (print the keys of `<tag>.json` whose value still equals the
  Russian source — derived, writes nothing).
  The module docstring documents the two-step contract, the naming convention, the
  suggested-key scheme, and the classification actions. Edit here to change
  classification rules, the suggested-key scheme, or catalog emission.
- `test_i18n_extract.py`: stdlib `unittest` suite. Covers lexing (raw strings,
  comments, char literals, escapes), classification precedence, `concat!` refusal,
  `format!`->`tf!` placeholder conversion, suggested-key stability/collisions, the
  mandated exclusion self-test, and the propose/apply-with-keys round trip (including
  the abort-on-missing-key guard). Run: `python3 -m unittest discover -s tools -p 'test_*.py' -v`.

## Contracts and invariants
- **No machine translation.** English catalog values are the Russian source text; a
  human fills the translations later. "Untranslated" is **derived, not stored**: a value
  still equal to the Russian source is untranslated. List them with
  `python3 tools/i18n_extract.py --untranslated <tag>` (`compute_untranslated`); `_meta`
  holds only `name`. The tool never writes a derived `untranslated` list into the catalog.
- **Migration scope is `src/`-only and absolute.** `in_migration_scope` gates
  every literal *before* any other rule: only `src/` (excluding `src/bin/`) is
  migrated. The GUI-free workspace crates (`crates/ms-*`, `ag-psd`, `puffin_egui`),
  build scripts, and standalone test trees classify as `SKIP_NON_UI_CRATE` and can
  never become `LOCALIZE_*`. This protects linguistic data — e.g. the Russian
  hyphenation word lists in `ms-text-util/.../cyrillic_slavic.rs` — from the
  `t!`-returns-key-on-miss trap that would silently corrupt behavior. `--apply`
  additionally refuses any path outside `src/` (or inside `src/bin/`). See
  `docs/i18n_exclusions.md` §F; enforced by `NonUiCrateScopeTest`.
  **Out of scope for the tool does not mean "needs no keys."** A GUI-free crate may
  still hand the binary a *label*; it returns a catalog key (e.g. `TextLanguage::name_key`)
  that the binary resolves via `src/i18n_resolve.rs`. Those keys are added to the five
  catalogs by hand, and the `ms-i18n` orphan guard scans `crates/` so a key returned by
  a crate but absent from `en.json` fails the build.
- **Hard exclusions are absolute.** `EXCLUSION_RULES` encodes
  `docs/i18n_exclusions.md` sections A / A4 / A5 / A5b / B (each rule carries a
  `doc` back-reference). §A5/§A5b cover default layer/group/baked/clip **names** that
  round-trip to `layers.json` or are written into an exported `.psd` (ps_editor,
  psd_export, and the shared `layer_model` fallback node names); a web-only rule
  excludes the demo chapter's virtual-FS storage path. The exclusion check runs
  after the scope check and short-circuits, so a listed identifier-like literal can
  never be localized by accident. Changing the exclusion doc means updating
  `EXCLUSION_RULES` (and vice versa).
- **`--apply` never corrupts nested literals.** When a Cyrillic literal sits inside
  a `format!` argument whose whole macro is itself converted to `tf!`
  (`format!("x {}", if c { "да" } else { "нет" })`), the inner edit span is contained
  in the outer one. `apply_to_file` drops the contained edit (leaving the literal
  as-is, valid Rust, flagged again for manual migration) instead of splicing one
  replacement into the middle of the other; `run_apply` warns about each dropped
  site. See `apply_to_file`'s overlap guard.
- **Human names the keys; the tool never does.** Final keys are supplied by a human
  in the `--keys` mapping file (see the naming convention above). The transliterated
  `module.fn.slug` scheme survives only as the advisory `suggested_key` in a
  proposal; `--apply` ignores it. A migratable hit with no supplied key aborts
  `--apply` (exit 2, no writes) — never an invented key.
- **`concat!` is never rewritten.** A Cyrillic fragment of a multi-part
  `concat!("…", "…")` is `REVIEW`: emitting `concat!(t!(…), …)` would not compile, so
  a human must collapse it into one key by hand.
- **`--dry-run` / `--propose` write no `.rs`.** Dry run writes nothing outside an
  explicit `--report`; propose writes only its `-o` proposal file.
- **`--apply` output is best-effort text surgery** and must be compiled + reviewed
  (notably the §C `id_salt` insertions and any `REVIEW`-flagged sites).

## Editing map
- To change what counts as a UI string / how sites are classified, see
  `classify_literal` and the callee-category sets (`LOG_CALLEES`, `ASSERT_CALLEES`,
  `KEYLIKE_CALLEES`, `is_widget_id_call`).
- A whole external test-module file (`tests.rs`, i.e. `#[cfg(test)] mod tests;`) is
  covered as one test span in `scan_file` via `is_test_module_file`, so test-helper
  fns without their own `#[test]` marker classify as `SKIP_TEST` (the per-file
  `test_spans` alone cannot see the parent's `#[cfg(test)]`).
- To change the migration scope (which trees are eligible at all), see
  `in_migration_scope` / `NON_UI_CRATE_DIRS` and `_reject_out_of_src` in `main`.
- To change the *suggested*-key scheme, see `module_path_for`, `base_key_for`,
  `slugify`, and `KeyRegistry` (advisory only — never a final key).
- To change the proposal / keys-file shape, see `build_proposal`, `load_key_mapping`,
  and `assign_keys_from_mapping` (call-site matching + abort-on-missing-key).
- To change `format!`->`tf!` conversion, see `convert_format`.
- To add/adjust an exclusion, see `EXCLUSION_RULES` and mirror
  `docs/i18n_exclusions.md`.
- To change catalog output, see `build_catalog` / `merge_catalog_file` (both emit only
  `name` in `_meta`; no derived `untranslated` list).
- To change how untranslated keys are listed, see `compute_untranslated` /
  `values_equal_to_source` / `run_untranslated` (the `--untranslated <tag>` mode).
- The generated audit lives at `docs/i18n_migration_report.md` (regenerate with
  `python3 tools/i18n_extract.py --dry-run --report docs/i18n_migration_report.md`).
