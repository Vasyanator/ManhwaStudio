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
`ms-storage`, `ms-docstore`, `ms-log`, `ms-sysprobe`, `ms-thread` and `ms-i18n`, and it may NOT depend on `app`,
`launcher` or another tab crate. Above it sit `app.rs` and `launcher/app.rs` (which own the
tab states) and `ms-tab-translation`, which reads this crate's note entries to build the MT
glossary.

The four tabs are independent of each other except for `notes`, which aggregates the entries
`characters` and `terms` expose. The owned title documents `characters.json` and `terms.json`
are read, written (atomically) and change-probed ONLY through `ms_docstore`; every other
markdown / text / image read and write goes through the `ms_storage::global::storage()` seam
so the web build can swap the backend. Portraits, note aggregation, wiki scan/markdown parse
and image decode run on an `ms_thread` worker and reach the GUI through an mpsc channel.

## Files and submodules
- `lib.rs`: crate root — mounts the `ms-i18n` macros and declares the four modules.
- `characters.rs`: character cards (portrait, aliases, description) over `characters.json`,
  plus the `load_character_names` / `load_characters_for_notes` readers other crates use.
- `terms.rs`: glossary CRUD over `terms.json`, plus the `load_terms_for_notes` reader.
- `notes.rs`: translation-notes tab with two sub-tabs — the prompt assembled on a worker from
  the `notes_file` template with `{charas}` / `{terms}` filled from characters and terms, and an
  editor for that template itself (with placeholder helpers). Inputs are re-probed every 600 ms.
- `wiki.rs`: bundled markdown documentation browser. Reads the per-language folder
  `wiki/<lang>/` chosen from the active UI locale (falling back to `wiki/en`, then `wiki/`)
  and re-scans when the interface language changes; pages share one `wiki/images/` tree via
  `../images/...` links, so image sources are normalized lexically. Also mounted by the
  launcher.

## Contracts and invariants
- No literal user-visible strings: every label goes through `t!` / `tf!` / `tp!`.
- Image decode and the heavy loads above are worker-driven. Known gap (CLAUDE.md §5): the
  characters/terms tabs still load and save their roster on the GUI thread
  (`dev-docs/known_gaps.md`); those saves use `Durability::None` (atomic, no fsync). The notes
  template editor likewise writes `notes_file` on the GUI thread (`save_template`).
- `characters.json` / `terms.json` go through `ms_docstore` only; other file access goes
  through the storage seam, never `std::fs` directly.
- A malformed `characters.json` / `terms.json` is reported and NEVER overwritten: not on
  load, and not by a later save — after a failed load the tab refuses every save/delete
  (`load_error`) until a successful reload, and the store-level save re-reads the existing
  document under its lock and refuses an unreadable or malformed one. The legacy
  `characters/*.txt` migration runs only when `characters.json` is absent, and its write is
  directory-fsynced before the `*.txt` sources are deleted.
- The wiki's remote-image loader is native-only (`ureq`); the wasm build compiles an error
  stub in its place.
- Tests that assert a `t!` / `tf!` / `tp!` rendering must install a catalog under
  `ms_config::locale_store::GLOBAL_LOCALE_LOCK` — the active catalog is one process-global
  `ArcSwap`, so those tests have to serialize. `ms-config` is a `[dev-dependencies]` entry
  with `test-support` for exactly this; it is deliberately absent from `[dependencies]`.

## Editing map
- To change character data or portraits, see `characters.rs`.
- To change the glossary, see `terms.rs`.
- To change what the notes prompt composes or the template editor, see `notes.rs` (and the two
  readers it calls).
- To change wiki rendering, markdown parsing or the per-language folder rules, see `wiki.rs`.
