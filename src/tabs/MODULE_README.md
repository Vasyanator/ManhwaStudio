# Module: src/tabs

## Purpose
Editor tab layer for project-scoped workflows. This directory owns the concrete tab modules that
`MangaApp` switches between after a chapter is opened.

## Architecture
`mod.rs` declares every tab module and the `AppTab` enum used by the root app, hotkey scopes, and
tab titles. Feature-heavy tabs are nested modules; lightweight data tabs are single files.

The shared canvas engine remains in `src/canvas/`. Translation, cleaning, and typing customize it
through `CanvasHooks` or typed canvas APIs. Shared persistent state lives in `src/models/` and
`ProjectData`; tab code should snapshot shared models before expensive work and release locks
before rendering, file I/O, image processing, AI requests, or worker waits.

Long-running work is worker-driven. Tab `draw` methods may render state, poll channels, and upload
already prepared textures, but must not block the GUI thread with project scans, image decode,
model/backend calls, text rendering, export, or synchronous save-heavy workflows.

## Files and submodules
- `mod.rs`: tab module wiring, `AppTab` values, tab order, and localized tab titles.
- `translation/`: OCR, text detection, machine translation, translation-side bubble editing, and
  translation-specific canvas hooks.
- `cleaning/`: clean-overlay editing tools, quick text cleanup, mask loading, and cleaning canvas
  behavior.
- `typing/`: text/image overlays, text rendering, typing masks, auto-typing, deformation, and
  export composition.
- `settings/`: project/user settings panes for general options, shared ribbon/canvas settings,
  AI backend process/device controls, and hotkey overrides.
- `characters.rs`: character database UI and image thumbnail/editor pipeline for project
  `characters.json` data.
- `terms.rs`: project `terms.json` CRUD UI and term data exported to notes.
- `notes.rs`: notes prompt/template editor that composes prompt text from notes, characters, and
  terms through a background worker.
- `wiki.rs`: local Markdown wiki viewer with worker-based scan, document load, and image decode.

## Contracts and invariants
- `AppTab::ALL`, `AppTab::title`, and root app tab routing must stay in sync when adding or
  removing tabs.
- Canvas behavior shared across tabs belongs in `src/canvas/`; tab-specific behavior should use
  hooks or narrow typed APIs instead of duplicating canvas state machines.
- Durable chapter data must flow through `ProjectData`, shared models, or explicit project path
  contracts. Do not hard-code project-relative paths that already exist in `ProjectPaths`.
- GUI-thread tab code must not perform blocking file I/O, image decode, AI/network requests,
  large parsing, export, or worker joins.
- Shared model locks must be short-lived and released before callbacks, canvas rendering, disk
  writes, or expensive computation.
- New maintained source subdirectories under `tabs/` need their own `MODULE_README.md`.

## Editing map
- To add a new top-level tab, update `mod.rs`, root `MangaApp` tab construction/routing, hotkey
  scopes if needed, and this document.
- To change OCR, detection, MT, or translation footer behavior, edit `translation/`.
- To change clean-overlay tools or quick-clean behavior, edit `cleaning/`.
- To change text overlays, renderer integration, masks, or export, edit `typing/`.
- To change shared ribbon/canvas settings, AI backend controls, or hotkey UI, edit `settings/`.
- To change character/term/notes/wiki workflows, edit the matching single-file tab.
