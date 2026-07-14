# Module: tools/egui_docs

## Purpose
Generates and guards `egui-docs/api/` — the machine-extracted half of the project's
egui reference. It exists so that "does this egui API exist in our version?" is
answered by grepping a committed file rather than by recalling a version of egui the
project does not use.

Nothing in the output is authored: every signature, doc line, and source location is
extracted from rustdoc JSON built from the exact crate sources cargo compiles against.

## Architecture

```
Cargo.lock ──pins──> egui/eframe/epaint/emath/ecolor/egui_extras 0.35.0
                          │
   cargo +nightly doc --output-format json   (build.sh)
                          ▼
                   target/doc/*.json         (rustdoc JSON, format_version 60)
                          │
                 gen_api_index.py
                          ▼
        egui-docs/api/*.md, api/symbols.txt, egui-docs/VERSION
                          │
                  check_sync.py  ──fails if──> VERSION != Cargo.lock
```

The one non-obvious step is re-export resolution. rustdoc records where an item is
*defined* (`egui::containers::panel::Panel`, `ecolor::Color32`), but callers write the
*re-exported* path (`egui::Panel`, `egui::Color32`). Indexing by definition path would
make a grep for `egui::Color32` come up empty and tell an agent the type does not
exist — precisely the failure this tooling prevents. `gen_api_index.py` therefore walks
each crate's public module tree and, via `Registry`, stitches cross-crate re-exports
back to the crate that owns them, so citations point at real source.

## Files
- `build.sh`: the entry point. Builds rustdoc JSON on nightly, runs the generator, then
  the sync check. Run it after any egui/eframe upgrade.
- `gen_api_index.py`: rustdoc JSON -> `egui-docs/api/`. Holds the type/signature
  printer, the public-path walker, and `Registry` (cross-crate re-exports).
- `check_sync.py`: fails when `egui-docs/VERSION` disagrees with `Cargo.lock`. Cheap,
  dependency-free; run it in any checkout.

## Contracts and invariants
- **Generated output is never hand-edited.** `egui-docs/api/**` and `egui-docs/VERSION`
  are overwritten wholesale. Prose belongs in the hand-written pages `egui-docs/0*.md`.
- **The output is committed.** Reading the reference must not require nightly or a doc
  build; only *regenerating* it does.
- **rustdoc JSON is an unstable format.** `EXPECTED_FORMAT_VERSION` (currently 60) is
  checked at load; a mismatch is a hard error, not a warning, because a silently
  misread schema would emit confident nonsense.
- Only the six egui-family crates are indexed. Re-exports pointing outside that set
  (std, ahash, serde) are logged and skipped, not faked.
- Requires the `nightly` toolchain (rustdoc JSON is nightly-only) and `python3`. The
  generator has no third-party Python dependencies.

## Editing map
- To index another crate, add it to `CRATES` in `gen_api_index.py` and to the `-p` list
  in `build.sh`.
- To change what a symbol line looks like, see `render_symbols()`.
- To change the per-crate page layout, see `render_crate()` / `render_item()`.
- If a future nightly bumps the rustdoc schema, re-read the JSON shape, update the type
  printer, then bump `EXPECTED_FORMAT_VERSION`.
