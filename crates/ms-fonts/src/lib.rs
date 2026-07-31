/*
File: crates/ms-fonts/src/lib.rs

Purpose:
Crate root of `ms-fonts` — the single owner of the bundled `fonts/ui` font stack.
It resolves the directory once, describes every font file in it (fallback order,
tier, family name) and hands out font bytes that live for the whole process.

Main responsibilities:
- expose the parsed-once manifest (`stack`) and the process-wide byte store (`bytes`);
- keep the stack free of any toolkit or shaping dependency, so both the egui UI (the
  binary crate) and the cosmic-text renderer (`ms-text-render`) can share one copy of
  the bytes and one set of family names.

Key structures:
- `Tier`: which stage of the stack a font belongs to.
- `StackFont`: one described font file.
- `FontStack`: the whole manifest of one `fonts/ui` directory.

Key functions:
- `stack`: the process manifest, resolved on first use.
- `bytes`: the bytes of one stack font, read once and extended to `'static`.

Notes:
No egui and no cosmic-text dependency, on purpose: `ms-text-render` must not depend on
the binary crate, so the base the UI and the renderer share has to live in a crate of
its own (`dev-docs/unicode_base_font_plan.md`, layer 0).
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

mod family_name;
mod manifest;
mod store;

pub use manifest::{FontStack, StackFont, Tier, stack};
pub use store::bytes;
