/*
File: crates/ms-tabs-simple/src/lib.rs

Purpose:
Crate root of the four small project-editor tabs — «Персонажи», «Термины», «Заметки»
and «Вики». Re-exported by the binary from `src/tabs/mod.rs` as
`crate::tabs::{characters, terms, notes, wiki}`, so every existing call site keeps its
path.

Layer:
Near the top of the library stack: above `ms-project`, `ms-widgets`, `ms-storage`,
`ms-sysprobe` and `ms-thread`, and below `app.rs` / `launcher/app.rs`, which own the tab
states, and below the `translation` tab, which reads this crate's note entries. The
crate must never name `app`, `launcher` or another tab.

Modules:
- `characters`: character cards (portrait, aliases, description) plus the
  `load_character_names` / `load_characters_for_notes` readers used by translation.
- `terms`: glossary terms plus the `load_terms_for_notes` reader used by translation.
- `notes`: the aggregated read-only notes view over `characters` and `terms`.
- `wiki`: the bundled markdown documentation browser (also mounted by the launcher).

Notes:
The four tabs are independent except for `notes`, which reads the other two. They share
one crate because their dependency sets are identical and splitting them would only
deepen the build graph.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/main.rs` mounts them for the binary: these tabs' localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

pub mod characters;
pub mod notes;
pub mod terms;
pub mod wiki;
