# Module: fonts/ui

## Purpose
Bundled font chain for the application's own user interface — every window the app opens
(studio, launcher, installer, updater, startup prompts) installs its fonts from here.

This is NOT the user's typesetting font library. Chapter text rendered onto pages uses the
fonts of the typing tab (`fonts/` next to a project / title, loaded by
`src/tabs/typing/fonts.rs`), and the whole `fonts/ui` subtree stays out of that scan
(`fonts::should_skip_font_dir`).

ONE exception, and it is a single list entry, not a folder: the typing font combo offers
the stack as a whole under the name «Встроенный шрифт интерфейса». That entry points at
`core/00-NotoSans-Regular.ttf` as its selected face and gets the rest of the chain from the
renderer's `common_fallback` (`src/tabs/typing/panel/MODULE_README.md`, "Built-in interface
font"). The files themselves are still never listed individually.

## Architecture
The directory itself is owned by `crates/ms-fonts`: it resolves which `fonts/ui` is the
stack, lists the files and hands out their bytes, read at most once per process. Everyone
else goes through that manifest — no other module may load these files.

There are exactly four readers, and they consume the same manifest for different roles:

- `crates/ms-fonts` — the OWNER. Resolves the directory, sorts the files by their `NN-`
  prefix, reads the family names out of each `name` table and hands out `'static` bytes.
- `src/ui_fonts.rs` — the egui UI. Runs on a worker thread and registers each file with
  `egui::Context::add_font` (as `FontData::from_static` over the shared bytes). It is also
  the only reader that honours a title-local `fonts/ui` override.
- `crates/ms-text-render/src/font_base.rs` — the text RENDERER (phase 4 of
  `dev-docs/unicode_base_font_plan.md`). Turns the same manifest into the renderer's own
  `fontdb::Database` plus the deterministic `MsFallback` chain, and never uses the
  operating system's fonts. It ignores the title override on purpose (decision 2).
- `src/tabs/typing/panel/{fonts,font_provider}.rs` — the typing panel (phase 5). Reads only
  `core[0]` (its path for the list entry, its `ms_fonts::bytes` for the renderer) to offer
  the stack as the selectable "built-in interface font". It uses the process manifest, not
  the title override, for the same reason the renderer does.

```text
fonts/ui/
  core/   always installed   (~19 MB)  latin, cyrillic, CJK, symbols
  bold/   always installed   (~0.6 MB) bold faces for the UI bold family
  ext/    studio, ON DEMAND  (~80 MB)  everything else: extra scripts, math, music, emoji
```

Which folders are installed is decided by `ui_fonts::Tier`:

- `Tier::Core` — `core/` + `bold/`. Launcher, installer, updater and the small startup
  prompt windows. They show short UI strings and never display chapter text.
- `Tier::Full` — `core/` + `bold/`, and `ext/` ARMED but not loaded. Studio window only.
  The extended tier is installed (in the background, additively) the first time
  `ui_fonts::ensure_covers` sees a character the installed chain cannot draw. Every surface
  that shows CHAPTER TEXT must offer its string: the canvas offers every bubble text, and
  the typing tab offers the overlay-creation field, the text accordion and the layers-list
  row previews. Most chapters never trigger it, so its
  ~80 MB stay off the heap; once triggered, the whole tier is loaded at once, because
  epaint parses `font_data` as a unit and has no per-glyph or per-file laziness.

The resulting egui families are `Proportional`, `Monospace`, `Name("system-ui-sans-bold")`
and `Name("canvas-bubble-unicode")`; the last two names are the constants
`ui_fonts::UI_BOLD_FAMILY_NAME` / `ui_fonts::BUBBLE_TEXT_FAMILY_NAME`.

## Files and submodules
- `core/`: the base chain. `00-NotoSans-Regular` first (it owns latin/cyrillic), then
  `01-SourceHanSansK-Regular` (CJK), then the two `NotoSansSymbols` faces.
- `bold/`: bold faces only. They are placed in FRONT of the core fonts inside the bold
  family, so core still covers glyphs the bold faces lack (in regular weight) instead of
  rendering tofu.
- `ext/`: pure fallbacks appended after everything else — math (`10-`), music (`11-`),
  emoji (`12-`), hentaigana (`20-`), one `NotoSans<Script>` per extra writing system
  (`30-`), Plangothic (`80-`/`81-`) and HanaMin (`90-`/`91-`) for rare CJK planes.

## Contracts and invariants
- **The `NN-` prefix is the fallback order of the text families.** Files are sorted by the
  numeric prefix, and the chains of `Proportional`, `Name("canvas-bubble-unicode")` and
  `Name("system-ui-sans-bold")` match that sorted order: a lower number is consulted first.
  Files without a valid `NN-` prefix sort last.
- **`Monospace` is the one documented exception.** Core fonts join it as a trailing fallback
  *after* egui's `Hack` (so paths and code keep the monospaced face), and their order among
  themselves is the REVERSE of the `NN-` order — `03`, `02`, `01`, `00`. The loader walks the
  sorted core list backwards because the text families insert at the FRONT
  (`FontPriority::Highest` is `fam.insert(0, ..)`, egui-0.35.0/src/context.rs:554), and the
  same single walk also feeds the `Lowest` append into `Monospace` (`fam.push`, :555).
  Un-reversing it would mean registering the core files a second time under different names —
  a repeated `add_font` with the same name is a no-op (egui-0.35.0/src/context.rs:2065-2076) —
  i.e. a second copy of ~19 MB of font bytes. That is not worth paying: inside `Monospace` the
  core faces overlap on only a handful of geometric symbols (■ ○ ● ◊), so the difference is
  cosmetic. The order is pinned by a unit test in `src/ui_fonts.rs`; change it deliberately or
  not at all.
- **A `fonts/ui` candidate must actually contain USABLE core fonts to win.** A candidate that
  yields no core font (`core/`, or the legacy flat layout) is skipped and logged, so an
  existing but empty — or `ext/`-only — folder cannot shadow the healthy bundled directory.
  "Usable" means the file really parses as a font and its family name reads: epaint PANICS on
  a font it cannot parse (`epaint-0.35.0/src/text/fonts.rs:987-1000`), so a title-local
  override — which arrives with untrusted project data — is validated before installation,
  and a file that fails is dropped with a logged reason instead of taking the studio down.
  The probe order is split by owner: the process manifest (`ms-fonts`) probes the working
  directory and then the executable directory, while a title-local `fonts/ui` is probed
  first and ONLY by `src/ui_fonts.rs`. That override therefore restyles the UI but never
  the render — a project must not be able to change how a finished render looks
  (`dev-docs/unicode_base_font_plan.md`, decision 2). An override supplies its own tiers
  only; a tier it does not ship stays empty rather than being mixed with the bundled one.
- **The separator is a HYPHEN.** It used to be a colon (`0:Roboto-…`), which is not a legal
  file-name character on Windows. A name with a colon has no prefix and therefore sorts last.
- Supported extensions: `.otf`, `.ttf`, `.ttc`, `.otc`. Anything else in these folders
  (this file included) is ignored.
- Numbers do not have to be contiguous and may repeat only if the tie order does not matter
  (a tie falls back to the lowercased remainder of the file name).
- A font that covers latin must NOT be given a low number in `ext/`: `ext/` fonts are
  appended as last-resort fallbacks, but a broad-coverage face placed early in `core/`
  would take over text meant for another face. That is exactly the bug the numbering fixes —
  `NotoSerifHentaigana` covers all of ASCII and used to swallow the whole UI.
- **A file whose PostScript name contains `Emoji` is special to the RENDERER.** cosmic-text's
  face filter short-circuits to "matches" for such a face regardless of style and stretch
  (`cosmic-text-0.14.2/src/attrs.rs:322-327`), so `ext/12-NotoEmoji-Regular.ttf` sits in the
  match set of literally every request. Consequence: a request no other bundled face can
  serve resolves to Noto Emoji and renders as `.notdef` tofu instead of failing loudly. The
  renderer guards against that (`crates/ms-text-render/src/font_registry.rs`,
  `family_has_matching_face`); do not "fix" it by renaming the emoji file.
- Keep `core/` small. It is loaded by every window in the app, including the installer.
- The folders are shipped: the root `.gitignore` is a publication allowlist and
  `!fonts/ui/**` is on it, so anything added here becomes part of the release.

## Editing map
- To add a script that only chapter text needs: drop a `30-NotoSans<Script>-Regular.ttf`
  into `ext/`. No code change.
- To change which faces the whole UI is drawn with: edit `core/` (and `bold/` for the bold
  weight), keeping the numbering consistent.
- To change WHEN a folder is loaded into the UI, the egui family names, or the on-demand
  rule for `ext/`: `src/ui_fonts.rs`.
- To change how these files back a RENDER (which tier is resident, the script -> font
  chains, the forbidden list): `crates/ms-text-render/src/font_base.rs`.
- To change which directory is the stack, the `NN-` ordering rule, or how the bytes are
  shared: `crates/ms-fonts/src/MODULE_README.md`.
