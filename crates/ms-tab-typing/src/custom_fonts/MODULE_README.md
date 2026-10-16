# Module: crates/ms-tab-typing/src/custom_fonts (crate `ms-tab-typing`)

## Purpose
The typing tab's runtime side of custom fonts. A custom font is DEFINED by its recipe
(`ms_custom_fonts::CustomFontRecipe`, stored in `fonts_data.json` for saved fonts or in a
layer's `text_params.font_recipe` for ad-hoc ones); the compiled TrueType face is a disposable
derivative. This module turns recipe + resolved sources into a face, caches it in memory and on
disk, and owns the custom-font name namespace. The compiler itself lives in `ms-custom-fonts`
(GUI-free); nothing here compiles outlines.

## Architecture
```text
font model (panel: custom_font_entry list build + previews, TabFontProvider) and hybrid_editor/
  -- TabFontProvider implements SourceResolver, calls -->
service::service()  (process-global CustomFontService)
  memory LRU (by RecipeKey, byte budget)  ->  disk_cache (fonts/custom_cache/<key>.ttf)
  -> ms_custom_fonts::compile (one job per key, ms_thread workers / the caller's worker)
adhoc: recipe <-> render name `MSCustom-adhoc-<16 hex>` (process-global interner)
identity: every `MSCustom…` name shape (one owner)
export: PSD export (panel snapshot -> export worker) -> service faces -> names + bundled .ttf
        (bundle_plan: a saved font's own pairs baked in via ms_custom_fonts::with_kerning_overrides)
```

## Files and submodules
- `identity.rs`: the namespace. Saved identity `MSCustom-<16 hex>` (random, `new_saved_identity`),
  persisted ad-hoc identity `MSCustom-adhoc-hybrid`, ad-hoc render name
  `MSCustom-adhoc-<16 hex>`, compiled faces `MSCustom-<8 hex>` / `MSCustomPreview-…`
  (`compiled_face_post_script_name` delegates to `RecipeKey::full_face_post_script_name`),
  `is_reserved_custom_identity`, and `is_adhoc_identity` — the ONLY test of "is this the
  persisted ad-hoc identity" (schema, presets, `font_admin`, panel all call it).
  `is_minted_saved_identity` is the exact, NON-normalizing test of the minted spelling
  (`fonts_data` decode prefers it when keys collide after normalization).
- `sources.rs`: `ResolvedSource`, the `SourceResolver` trait the font model implements
  (`panel::font_provider::TabFontProvider`),
  `SourceError`, and `user_kerning_per_mille` (store pairs -> compiler pairs, one seam).
- `adhoc.rs`: `render_name` / `recipe_for_render_name` — how a name-only `FontProvider` finds an
  ad-hoc layer's recipe.
- `service.rs`: `CustomFontService` / `service()`, `CompiledFace`, `CompileTicket`,
  `CustomFontError`, and `blend_stats_summary` (the `blend paths: …` suffix of the one
  "compiled custom font" log line every full compile writes, from `CompiledFont.stats`);
  tests in `service_tests.rs`.
- `disk_cache.rs`: the cache directory, validated load (mtime touch), atomic store, `prune`,
  and the checked-removal protocol (move aside, re-check, delete or restore).
- `export.rs`: custom fonts in the PSD export (decision D2). `CustomFontUseKey` (saved identity
  or canonical ad-hoc recipe JSON), `custom_font_use_of_render_data`, and
  `CustomFontExportResolver` — the snapshot the panel puts into a PSD export request
  (`panel/facade.rs::custom_font_export_resolver`: saved recipes of the list + the panel's
  source resolver) whose `resolve_chapter` yields one face per use key (with each saved font's
  own kerning pairs, read from the font settings store on the worker) plus failures, and
  `CustomExportFaces::bundle_plan` — the `.ttf` files to write (`BundledFontFile`) and the
  `BundleIssue`s to report. `test_support` holds the fake-compiler fixtures (and a real compiled
  face over the tracked bundled fonts) shared with `tab/export.rs` tests.

## Contracts and invariants
- One identity namespace, one owner: no other file spells an `MSCustom` prefix. The three
  shapes are pairwise disjoint (length + the `adhoc-` marker). A USER font whose identity falls
  in the namespace must never be registered under it (the font list suffixes it).
- Keys: `ms_custom_fonts::RecipeKey` (SHA-256) is the only disk key. `font_content_id`
  (`DefaultHasher`) keys in-process memos only and is never persisted.
- Threading: `full_face_blocking`, `subset_preview_blocking`, `recipe_key` and
  `prune_disk_cache` block (hashing, disk I/O, compile) — worker threads only. The GUI thread
  uses `start_full_compile` (+ `CompileTicket`), `peek_full`, `request_disk_load` and
  `start_disk_prune_once`. The service's locks guard map lookups only, never I/O or compiles;
  lock order is job state -> ticket state.
- Jobs: one in-flight job per key; later callers join it. A job is cancelled only when every
  ticket that joined it cancelled and no blocking caller waits; blocking callers never cancel.
  Dropping a ticket detaches without cancelling. A leader that panics publishes `Worker`.
- Memory LRU: accounted in bytes (`DEFAULT_MEMORY_BUDGET_BYTES` = 256 MiB), the newest face is
  always kept even if it alone exceeds the budget; an evicted face lives on while held.
- Previews never compile a full face: they use `peek_full` / `request_disk_load`, or the
  uncached `subset_preview_blocking` (decision D5).
- Disk cache (decision D10): `ms_config::storage_mode::app_fonts_dir()/custom_cache/`, `None` in
  a test process (`font_settings_store::persistence_suppressed_by_tests`: no read, no write).
  The process-global service decides the directory on EVERY access (`CacheLocation::Production`),
  so a test latch armed after `service()` was created still holds; GUI-thread guards use
  `cache_enabled` (no file-system stat). A file is trusted only if it is the compiled full face
  of exactly its key (`RecipeKey::verify_full_face`: PostScript name AND the full-key unique
  id); otherwise it is removed and recompiled (logged). Removal (invalid load, prune) never
  deletes a face another app process published meanwhile: the file is renamed aside, the moved
  file re-checked (same rejected bytes / still stale) and only then deleted, else put back
  without replacing a newer publication; the residual is a transient miss (one recompile) for a
  concurrent reader. A hit touches the mtime. Prune keeps referenced keys, keys
  in memory / in flight, and any file touched within 14 days (`PRUNE_GRACE`; protects ad-hoc
  faces of closed chapters); it runs once per process (`start_disk_prune_once`, started by the first panel list build,
  `panel::fonts::load_fonts`, with the keys of every listed saved font). The folder
  scanner skips this directory (`panel::fonts::should_skip_font_dir`).
- PSD export (decision D2): a custom-font text layer is written as a normal text layer named by
  its compiled face's PostScript name (`CompiledFace::post_script_name`; nobody spells the
  format). The export worker (`tab/export.rs::prepare_custom_font_export`) resolves every font
  the chapter's layers use once, installs the names into each job's `FontPostScriptNames`
  (`psd_export.rs` reads them before its usual chain) and, with «Положить рядом кастомные
  шрифты» (`TextTab.psd_bundle_custom_fonts`), writes each distinct face (dedup by
  `RecipeKey`) as `<dir>/<PostScript name>.ttf` before the destination commit. A font without
  a face is a warning, never an aborted export; its layers keep the stored identity.
- Own kerning in the PSD bundle: the cached face (and its `RecipeKey`) carries the INHERITED
  kerning only; the app applies a saved font's OWN pairs at render time through the provider.
  Photoshop applies only what the file declares, so `bundle_plan` writes a saved font with own
  pairs as a COPY with them baked into `kern` (`ms_custom_fonts::with_kerning_overrides`: the
  renderer's replace-not-add rule, `0` cancels, unmapped characters ignored; names unchanged,
  so the PSD layers still resolve). Pairs come from `font_settings_store::font_custom_kerning`
  by the saved identity, read on the export worker (`facade.rs` passes no pairs). One face used
  with DIFFERENT own pairs (two saved fonts with equal recipes, or a saved font and an equal
  ad-hoc recipe: one name, one file) bakes the pairs of the smallest saved identity that has
  pairs and reports `BundleIssue::KerningConflict`; a failed bake writes the compiled bytes and
  reports `KerningNotApplied`. Both become warnings, never an aborted export.
- Errors: `CustomFontError` `Display` is log text; UI code maps variants to i18n keys. Disk
  cache failures are logged and never fail a face (they only cost a recompile).

## Editing map
- New name shape or reserved prefix: `identity.rs` (+ its disjointness test).
- How sources are resolved / which user kerning counts: the font model's `SourceResolver`
  impl (`panel/font_provider.rs`) and `panel::custom_font_entry::resolve_recipe_sources`; the
  unit conversion is `sources.rs`.
- How saved custom fonts become list entries, previews and rows: `panel/custom_font_entry.rs`
  (see "Custom fonts in the font model" in `panel/MODULE_README.md`).
- Cache policy (budget, grace, validation): `service.rs` constants, `disk_cache.rs`.
- How the compiler is invoked (scope, progress, cancel): `service.rs` only.
- PSD naming / bundling of custom fonts: `export.rs` (resolution, own-kerning bake plan),
  `tab/export.rs` (`prepare_custom_font_export`, file writing, warnings), `psd_export.rs`
  (layer font name), the checkbox in `panel/create_sections.rs`, persistence in
  `tab/studio_tab.rs`; the `kern` rewrite itself is `ms-custom-fonts` (`kern_override.rs`).
