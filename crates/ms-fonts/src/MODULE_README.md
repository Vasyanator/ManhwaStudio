# Module: crates/ms-fonts/src

## Purpose
Single owner of the bundled `fonts/ui` stack: it decides WHICH directory is the stack,
WHAT is in it (order, tier, family name) and hands out the font bytes. Everything else in
the project is a consumer — the egui UI in the binary crate and, from phase 4 on, the
cosmic-text renderer in `ms-text-render`.

The crate depends on neither egui nor cosmic-text, and that is the point: `ms-text-render`
must not depend on the binary crate, so the base the two of them share has to live in a
crate of its own (`dev-docs/unicode_base_font_plan.md`, layer 0).

## Architecture
Two independent halves behind one small API.

- **Manifest** (`manifest.rs`) — resolved once per process into a `OnceLock`:
  `fonts/ui` candidates (launch working directory, then executable directory) are probed
  in order, and the FIRST one that actually yields core fonts wins. A directory that
  merely exists is not enough, so an empty or `ext`-only folder cannot shadow the healthy
  bundled one. Each rejected candidate is logged with its reason. The winner's `core/`,
  `bold/` and `ext/` are listed, sorted by the `NN-` file-name prefix, and described as
  `StackFont` records.
- **Byte store** (`store.rs`) — a `Mutex<HashMap<PathBuf, &'static [u8]>>` keyed by FILE
  IDENTITY (the canonical path): a file is read at most once per process and its bytes are
  then extended to `'static`. Every spelling ever asked for is kept as an alias entry
  pointing at the same slice, so a repeated call needs no `canonicalize` syscall. If
  canonicalization fails the store degrades to the path as written (logged only when the
  file itself IS readable, so an unreadable font does not produce two lines per call).

```text
stack() -> FontStack { core, bold, ext }   // paths + orders + family names
bytes(&StackFont) -> &'static [u8]         // read once, shared by every consumer
```

Both are lazy: nothing is read until a consumer asks.

`ms_fonts::Tier` names WHERE a font comes from (`core/`, `bold/`, `ext/`) and is not the
same type as the UI-side `ui_fonts::Tier`, which names HOW MUCH of the stack a window
installs (`Core` = core + bold, `Full` = everything).

## Files and submodules
- `lib.rs`: crate root; declares the lints and re-exports the whole public API
  (`Tier`, `StackFont`, `FontStack`, `stack`, `bytes`).
- `manifest.rs`: directory resolution, tier listing, `NN-` ordering, the `OnceLock`
  manifest. Edit it for layout, candidate or ordering rules.
- `family_name.rs`: reads the family name out of a font's `name` table, walking the sfnt
  structure by hand so only a few kilobytes are read per file (a collection is described
  by its face 0). Edit it for name-selection rules.
- `store.rs`: the process-wide byte store. Edit it for how bytes are read and shared.

## Contracts and invariants
- **The family name is never guessed.** It comes from the `name` table, and the selection
  rule mirrors `fontdb::parse_names` (typographic family, else family, US-English
  preferred). A file whose name cannot be read is logged and LEFT OUT of the stack rather
  than named after its file — a fabricated name would be silently unreachable in every
  fallback chain, and `fontdb` refuses such a file the same way.
- **Family names and font bytes are `'static` on purpose.** The cosmic-text `Fallback`
  trait can only name a family through `&'static str`, and neither `epaint` nor
  cosmic-text can unload a font once registered; the real lifetime of both is the lifetime
  of the process. Both leaks are bounded: the manifest is built once, and each file is
  read at most once.
- **`bytes()` is idempotent by address, per FILE.** Two calls for the same file return the
  SAME slice even when the path is spelled differently (relative/absolute, a `..`
  component, a symlink), so consumers must not cache it themselves, and
  `FontData::from_static` / `fontdb::Source::Binary` share one copy instead of each
  holding their own. Keying by spelling would leak one process-lifetime copy per spelling.
- **No project/title override here.** The manifest is process-global, so it must not
  depend on the open project: a title-local `fonts/ui` applies to the UI only, because a
  project must never be able to change how a finished render looks
  (`dev-docs/unicode_base_font_plan.md`, decision 2).
- **Nothing here may run on the GUI thread on first use.** `stack()` lists directories and
  reads a `name` table per file; `bytes()` reads whole font files. Later calls are cheap
  (an atomic load, resp. a hash lookup).
- **Failure is always partial and logged.** A missing directory, an unreadable entry, an
  unparsable font or a failing `current_dir`/`current_exe` removes exactly that one item
  and is logged through `ms-log` with the operation and the path; nothing panics and no
  error is swallowed.

## Editing map
- To change which directories are searched, or the layout inside one, see
  `manifest.rs::candidate_dirs` / `probe_core_paths` / `collect_tier_paths`.
- To change the fallback order rule (`NN-` prefix), see `manifest.rs::font_sort_key`.
- To change how a font is named for shapers, see `family_name.rs::select_family`.
- To change how bytes are read, cached or shared, see `store.rs::bytes_at_path`.
- The bundled files themselves and their `NN-` numbering are documented in
  `fonts/ui/MODULE_README.md`.
