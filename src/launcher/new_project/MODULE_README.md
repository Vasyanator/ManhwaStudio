# Module: src/launcher/new_project

## Purpose
Detached "New Project" launcher window. This module gathers source pages, prepares ribbon images,
downloads or imports external chapter content, and saves the resulting project without blocking the
main launcher UI.

## Architecture
UI-facing state lives in `window.rs`. It owns the native/embedded viewport, form state, ribbon
preview, crop/manual-cut UI, and event polling. Long-running behavior is split into controller
modules that expose `begin`/`poll` style APIs over channels: source import, project catalog/save,
quick download, advanced Selenium download, stitching, waifu2x, clipboard/screen capture, and
batch processing.

Image import normalizes external files into `ImportedImage` and `RibbonPage` values from
`ribbon.rs`. File image formats are detected from file signatures before decoding; extensions are
used only for picker filters, archive type selection, and lightweight name ordering where bytes are
not yet available.

The ribbon model is the in-memory handoff between acquisition and save/export. Import/download
modules produce decoded images, `ribbon.rs` builds tiled previews while preserving source pixels and
crop metadata, and `project_io.rs` writes selected images to `src`, `alt_vers`, or an arbitrary
folder. Browser automation uses the shared Python runtime path and a JSON-RPC helper daemon instead
of embedding Selenium in Rust.

## Files and submodules
- `mod.rs`: public module map for the detached new-project window.
- `window.rs`: `NewProjectWindowState`, viewport rendering, left-panel modes, ribbon preview,
  crop/manual cut UI, save forms, controller polling, and open-project handoff after save.
- `open_source.rs`: source picker and workers for folders, saved HTML, archives, and single image
  files; includes byte-signature image detection, natural ordering, filtering, and progress events.
- `project_io.rs`: projects-root catalog scan, target resolution, parallel PNG save pipeline, and
  save result mapping back to `OpenProjectSelection` where applicable.
- `quick_download.rs`: direct chapter downloader for supported sites, URL extraction, parallel
  image download/decode, and ribbon conversion.
- `advanced_download.rs`: Selenium/browser-profile JSON-RPC bridge through `adv_fetch_cli.py`,
  persistent daemon lifecycle, helper version checks, link collection, direct fetch, canvas
  snapshot, and canvas intercept workflows.
- `stitching.rs`: vertical stitch/split heuristics, heterogeneous-bottom adjacent-page merge,
  cut-like-reference, manual cut apply, progress events, and synchronous helpers reused by batch
  execution.
- `waifu2x.rs`: platform runtime discovery/download/extraction, dynamic shared-library loading,
  cached model/context lifetime, cancellation, worker processing, and synchronous helpers reused by
  batch execution.
- `ribbon.rs`: `RibbonState`, `RibbonPage`, `RibbonTile`, `RibbonCrop`, `ImportedImage`, tiled
  preview generation, adjacent page merge, non-destructive crop state, and original-page
  restoration.
- `batch_processing/`: standalone visual graph editor and executor for repeated import/download,
  browser, stitch, waifu2x, and save pipelines. See `batch_processing/MODULE_README.md`.

## Contracts and invariants
- GUI code must only poll worker state. Decoding, filesystem traversal, archive extraction,
  downloads, Selenium calls, image processing, screen/clipboard capture work, waifu2x runtime
  loading, and PNG saving stay off the main thread.
- Unsupported or unreadable sources must return user-facing errors plus detailed log messages; do
  not add fake pages or placeholder outputs.
- Imported pages preserve stable natural ordering before ribbon construction.
- Source images are decoded from real bytes, not from filename labels.
- Ribbon pages retain original pixels and crop metadata so crop/restore operations are
  non-destructive.
- Browser automation must go through `python_manager` and the project helper daemon; profile and
  Python-process lifecycle are owned by the downloader controller. The helper daemon is spawned
  through the managed Python child path so Windows kills it if the Rust parent dies.
- The advanced downloader helper must include `downloader_version` in daemon events; Rust compares
  it with `CARGO_PKG_VERSION` and shows a session-only warning on mismatch.
- Waifu2x must keep the application usable when the shared library is absent; the worker either
  downloads/extracts the real runtime or returns a clear error.

## Editing map
- To change source picking or image import, edit `open_source.rs`.
- To change direct supported-site downloading, edit `quick_download.rs`.
- To change Selenium/browser download, link collection, or canvas capture, edit
  `advanced_download.rs`.
- To change save/export behavior or project catalog scanning, edit `project_io.rs`.
- To change ribbon page state, tiling, adjacent page merge, crop metadata, or original-page restoration, edit
  `ribbon.rs`.
- To change stitch/split, heterogeneous-bottom merge, or cut behavior, edit `stitching.rs`.
- To change waifu2x runtime discovery, package download, cancellation, or processing, edit
  `waifu2x.rs`.
- To change detached window UI flow or controller wiring, edit `window.rs`.
- To change batch graph editing or execution, edit `batch_processing/`.
