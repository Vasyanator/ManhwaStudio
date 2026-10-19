# Module: crates/ms-tab-translation/src/neurolate

## Purpose
The view half of `../neurolate.rs` (the Translation tab's per-title NeuroLate settings and the
one owner of their "Auto" rules). The parent file owns the types, persistence and rules; this
directory holds only their widgets.

## Files and submodules
- `view.rs`: `draw_neurolate_settings` and `NeurolateSettingsView` — the language and comic-type
  pickers (each with an "Auto (...)" entry resolved through the parent's rules) and, for the OCR
  engine only, the "create mask and text blocks" checkbox. Drawn by the detector panel and the
  OCR panel over the tab's ONE `NeurolateSettings` store.

## Contracts and invariants
- Edits the caller's store in place and returns whether anything changed; starts no work, does
  no I/O.
- "Auto" labels resolve through `effective_language` / `resolve_comic_type` only, so the label
  is exactly what a request would send.
- The language picker is a catalog (every NeuroLate language), so it is a `SearchableComboBox`
  (code, plus the localized name where `panels::ocr_langs` has the ISO code; code alone
  otherwise); the type picker is a `WheelComboBox`. Ids derive from the caller's `id_salt`
  (one salt per panel); no user-visible literals (i18n keys under `translation.neurolate.*`).

## Editing map
- To change the pickers or the checkbox, edit `view.rs`; to change what "Auto" means or how the
  settings persist, edit `../neurolate.rs`.
