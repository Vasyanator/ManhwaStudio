# Module: crates/ms-tabs-simple/src

## Purpose
The four small, self-contained project-editor tabs: «Персонажи» (`characters`), «Термины»
(`terms`), «Заметки» (`notes`) and «Вики» (`wiki`). Each is a single file with the same
tiny dependency set; they share one crate deliberately, because four separate crates would
only deepen the build graph the crate split exists to flatten.

Re-exported by the binary from `src/tabs/mod.rs` as
`crate::tabs::{characters, terms, notes, wiki}`, so every pre-split call site keeps its path.

## Architecture
Layer: near the top of the library stack. It may depend on `ms-project`, `ms-widgets`,
`ms-storage`, `ms-sysprobe`, `ms-thread` and `ms-i18n`, and it may NOT depend on `app`,
`launcher` or another tab crate. Above it sit `app.rs` and `launcher/app.rs` (which own the
tab states) and `ms-tab-translation`, which reads this crate's note entries to build the MT
glossary.

The four tabs are independent of each other except for `notes`, which aggregates the entries
`characters` and `terms` expose. Every JSON / markdown / image read and write goes through
the `ms_storage::global::storage()` seam so the web build can swap the backend; every
non-trivial load (portraits, note aggregation, wiki scan and markdown parse, image decode)
runs on an `ms_thread` worker and reaches the GUI through an mpsc channel.

## Files and submodules
- `lib.rs`: crate root — mounts the `ms-i18n` macros and declares the four modules.
- `characters.rs`: character cards (portrait, aliases, description) over `characters.json`,
  plus the `load_character_names` / `load_characters_for_notes` readers other crates use.
- `terms.rs`: glossary CRUD over `terms.json`, plus the `load_terms_for_notes` reader.
- `notes.rs`: read-only aggregated notes view composed from characters and terms on a worker.
- `wiki.rs`: bundled markdown documentation browser. Reads the per-language folder
  `wiki/<lang>/` chosen from the active UI locale (falling back to `wiki/en`, then `wiki/`)
  and re-scans when the interface language changes; pages share one `wiki/images/` tree via
  `../images/...` links, so image sources are normalized lexically. Also mounted by the
  launcher.

## Contracts and invariants
- No literal user-visible strings: every label goes through `t!` / `tf!` / `tp!`.
- The GUI thread never does file I/O or image decode; all of it is worker-driven.
- File access goes through the storage seam, never `std::fs` directly.
- The wiki's remote-image loader is native-only (`ureq`); the wasm build compiles an error
  stub in its place.
- Tests that assert a `t!` / `tf!` / `tp!` rendering must install a catalog under
  `ms_config::locale_store::GLOBAL_LOCALE_LOCK` — the active catalog is one process-global
  `ArcSwap`, so those tests have to serialize. `ms-config` is a `[dev-dependencies]` entry
  with `test-support` for exactly this; it is deliberately absent from `[dependencies]`.

## Editing map
- To change character data or portraits, see `characters.rs`.
- To change the glossary, see `terms.rs`.
- To change what the notes view composes, see `notes.rs` (and the two readers it calls).
- To change wiki rendering, markdown parsing or the per-language folder rules, see `wiki.rs`.
