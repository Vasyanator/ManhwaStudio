# Module: crates/ms-tab-typing/src/hybrid_editor (crate `ms-tab-typing`)

## Purpose
The ONE editor body for hybrid custom fonts (`dev-docs/hybrid_fonts_plan.md` §3.3, decisions
D3, D4, D5, D9). Every host draws the same widget: the typing dock panel (a layer's ad-hoc
recipe) and Settings › Тайп (create dialog, properties window of a saved font). The body decides
nothing host-specific and persists nothing; the host says what its commit control is and what a
committed recipe means.

## Architecture
```text
host (typing dock / settings)            -- owns HybridEditorState + Arc<SourceCatalog>
  draw_hybrid_editor(ui, &mut state, &HybridEditorCx { catalog, commit })   (mod.rs, GUI)
    draft edits -> state.touch() (revision++)
    PreviewEngine::tick (preview.rs): drain results -> textures; coalesce 120 ms -> request
        worker "typing-hybrid-editor-preview" (one per editor, latest-wins):
          resolve sources (catalog resolver) -> comparison renders (memoized)
          -> coverage_report (ms-custom-fonts)  -> subset compile (service().subset_preview_blocking)
          -> render_text_to_image (white glyphs, display px) -> response + ctx.request_repaint
    commit: state.start_commit -> service().start_full_compile -> CompileTicket polled per frame
        -> HybridEditorOutcome::Committed(CommittedRecipe { recipe, face })
```

## Files and submodules
- `mod.rs`: `draw_hybrid_editor`, `HybridEditorCx`, `CommitButton` (`Apply` / `HostDrawn`),
  `HybridEditorOutcome`; the drawing of source rows, policies, preview and commit.
- `state.rs`: `HybridEditorState` (draft rows, default policy, preview text, revision, commit
  status, preview engine), `DraftError`, `CommittedRecipe`, and the pure helpers
  (`weight_shares`, override index mapping, `cycle_index`, row limits).
- `catalog.rs`: `SourceCatalog` / `SourceItem` — the allowed sources (bundled interface font
  first, real fonts; never a custom font) + their resolver + a process-unique serial.
- `preview.rs`: `PreviewEngine` (scheduler, slots, worker protocol), `LatestWins`,
  `preview_chars`, the worker (`run_worker` / `process`) and its render parameters.
- `coverage.rs`: `CoverageView` and its localized summary line.

## Contracts and invariants
- Sources (D3, D4): 2..=4 rows; the picker offers only the catalog, which never holds a custom
  font; the resolver refuses one as well (`TabFontProvider::resolve_source`).
- Draft validity is `ms_custom_fonts::CustomFontRecipe::validate` with the typing model's
  `normalize_font_identity` (duplicates compared normalized); an unfilled slot is `EmptySlot`.
- Missing glyphs (D9): each row has an override («как у шрифта / отключать символ / без
  влияния»), the hybrid a default (new drafts: `NoInfluence`). The editor never re-derives the
  rule: coverage counts come from `ms_custom_fonts::coverage_report`.
- Live preview (D5): every edit (rows, weights, policies, sample text) recompiles ONLY the
  sample's characters (uncached subset face), latest-wins: a newer request raises the previous
  one's cancel flag and bumps the shared generation that stops its renders; responses older than
  the image shown are dropped; the previous image stays until replaced. Requests also follow the
  catalog serial (a reloaded font list), the display scale and the quantized panel width.
- Commit: only `start_commit` compiles the FULL face (`start_full_compile`: memory / disk cache
  / compile, progress + cancel). `HybridEditorOutcome::Committed` is reported once; the committed
  recipe becomes the clean baseline (`is_dirty`). Failures are logged with the recipe JSON and
  shown as a localized line.
- Threading: the GUI thread never resolves, reads, compiles or renders here; it only sends
  requests, drains channels, uploads textures (`ColorImage` length checked first) and polls the
  ticket. `HybridEditorState` is `Send` (it lives inside tab state that moves to the teardown
  worker). Dropping the state ends its worker (channel closed), raises the in-flight preview's cancel
  flag and CANCELS a running commit (`impl Drop`): a commit's result reaches a host only through
  its editor, so every host (dock closed or rebound, settings dialog / window closed) stops it.
- No egui font is ever registered for a draft (egui cannot unload fonts); previews are textures.
  The picker rows use the fonts' own typefaces through `FontPreviewSource::request_family`.
- UI: no stock `Slider` / `ComboBox` / `DragValue`; every widget id is salted with an i18n key
  literal; all text is `t!` / `tf!` (`typing.hybrid_font.*`).

## Editing map
- Add a control or change the layout: `mod.rs`.
- Change draft rules, defaults, weight limits, commit behaviour: `state.rs`.
- Change what may be a source: `catalog.rs` (and `panel::custom_font_entry::hybrid_source_fonts`
  for the settings / list-build side).
- Change preview scheduling, render size or the worker: `preview.rs`.
- Change the coverage line: `coverage.rs` (counts themselves: `ms-custom-fonts` `hybrid/plan.rs`).
- A new custom font KIND gets its own editor module beside this one (one editor per kind).
