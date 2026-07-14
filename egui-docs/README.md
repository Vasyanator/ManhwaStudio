# egui-docs — the egui reference for this repo

**Read this before writing or changing any UI code.**

This project builds against **egui / eframe 0.35.0** (upstream crates.io, no fork,
no `[patch]`). egui 0.35 renamed, removed, or restructured a large part of the API
that a language model has memorised from earlier versions. Writing egui from memory
here does not produce slightly-old code — it produces code that **does not compile**,
or worse, code that compiles and behaves subtly wrong (input leaking under overlays,
widget state reset on language switch).

This folder exists so that no agent ever has to guess.

## The two rules

1. **Existence is decided by `api/symbols.txt`, not by memory.**
   If a name is not in that file, it does not exist in the version we build against.

   ```bash
   grep -P '^egui::SidePanel\t'  egui-docs/api/symbols.txt   # no hits -> does not exist
   grep -P '^egui::Panel::top\t' egui-docs/api/symbols.txt   # -> egui-0.35.0/src/containers/panel.rs:238
   ```

2. **Every API claim in these pages carries a `file:line` citation into the crate
   source.** An uncited claim is not authoritative — verify it before relying on it,
   and add the citation. When you write a new page here, you inherit this rule.

   Two kinds of citation appear, and they age differently:

   - `egui-0.35.0/src/…:LINE`, `epaint-0.35.0/…`, `emath-0.35.0/…` — into the crate
     sources in the local cargo registry. Pinned to an exact published version, so
     these do **not** drift. They are authoritative.
   - `src/…:LINE`, `README_AGENT.md:LINE` — into this repo. These drift as the code
     is edited. Trust the **path and the symbol name**; treat the line number as a
     hint that was correct when the page was written, and re-locate by name if it
     no longer lands where the page says.

## Where to look

| You are about to… | Read |
|---|---|
| write *any* egui code after a break | [`00-version-map.md`](00-version-map.md) — what you remember vs. what exists |
| add a window, panel, tab, or the app's `fn ui` | [`01-app-shell.md`](01-app-shell.md) |
| draw on a canvas: shapes, meshes, textures, images | [`02-painting.md`](02-painting.md) |
| handle clicks, drags, the mouse wheel, or hotkeys | [`03-input.md`](03-input.md) |
| add a widget, a settings pane, or a slider/combobox | [`04-widgets.md`](04-widgets.md) |
| localize a label, or store per-widget state | [`05-ids-and-i18n.md`](05-ids-and-i18n.md) |
| draw on top of other UI, or block input beneath something | [`06-overlays.md`](06-overlays.md) |
| *see* the running UI, or reproduce a UI bug | [`07-inspection.md`](07-inspection.md) |
| ask "does method X exist / what is its signature?" | [`api/`](api/README.md) |

## The three traps that bite hardest

Full detail in `00-version-map.md`; these are the ones worth knowing before you read
anything else.

- **`eframe::App` has no `update`.** The entry point is
  `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` —
  `eframe-0.35.0/src/epi.rs:176`. You are handed a `&mut Ui`, not a `&Context`.
- **There is no `SidePanel` / `TopBottomPanel`.** One unified
  `Panel::left/right/top/bottom(id)` — `egui-0.35.0/src/containers/panel.rs:180`.
  Panels take a `&mut Ui`, not a `&Context`.
- **Never read the raw pointer position to decide hover.** Use `Response::hovered()` /
  `contains_pointer()`. Raw-pointer reads leak straight through overlays and modals,
  which block input by z-order occlusion, not by disabling widgets. See
  `06-overlays.md` §5.

## Project rules that override egui defaults

These are not egui facts; they are this repo's contracts. Breaking them compiles fine
and still counts as a defect.

- **Do not use `egui::Slider` / `egui::ComboBox` / `egui::DragValue` directly in
  product UI.** Use the `Wheel*` widgets from `src/widgets/` (`README_AGENT.md`,
  §"Виджеты"). See `04-widgets.md` §0.
- **No literal user-visible strings.** Everything goes through `t!` / `tf!` / `tp!`.
  See `05-ids-and-i18n.md` §3.
- **A localized label requires a stable `id_salt`.** egui derives widget `Id` from the
  label text, so without a salt, switching language resets the widget's state. See
  `05-ids-and-i18n.md` §2.
- **The GUI thread never blocks.** No I/O, decode, or long compute inside `fn ui`
  (`README_AGENT.md`, "GUI Thread").

## Layout of this folder

```
egui-docs/
  README.md            this file — entry point and routing
  00-version-map.md    stale-memory API -> real 0.35 API, with removal proofs
  01-app-shell.md      eframe::App, panels, viewports, startup, fonts, theme
  02-painting.md       Painter, Shape, Mesh, Color32, ColorImage, textures
  03-input.md          InputState, pointer, Sense, Response, keyboard, hotkeys
  04-widgets.md        the project widget set; atoms; the forbidden egui widgets
  05-ids-and-i18n.md   Id derivation, id_salt, localization contract
  06-overlays.md       Area/Order, z-order input occlusion, the tutorial engine
  07-inspection.md     driving the live app over the egui inspection protocol
  api/                 GENERATED. Full public API index + symbols.txt
  VERSION              GENERATED. Crate versions these docs describe
```

Pages `00`–`07` are hand-written and source-cited. `api/` and `VERSION` are generated
and must never be edited by hand.

## Keeping this true

The docs are only worth reading while they describe the egui we actually compile
against. A stale reference is worse than none, because it is trusted.

```bash
python3 tools/egui_docs/check_sync.py   # fails if egui-docs/VERSION != Cargo.lock
tools/egui_docs/build.sh                # regenerate api/ + VERSION (also runs the check)
```

**Upgrading egui/eframe is not done until `tools/egui_docs/build.sh` has been run and
its diff reviewed.** The script regenerates `api/` and `VERSION` for you, but it
cannot re-check the prose: the hand-written pages must be re-verified against the new
source by hand — `00-version-map.md` first, since it is the page an upgrade is most
likely to silently falsify.

Version described: see [`VERSION`](VERSION). Sources are the exact crates in the local
cargo registry (`~/.cargo/registry/src/index.crates.io-*/egui-0.35.0/` and siblings);
upstream commit `6f15dc0e16b26edce1fc2a05212eaf7e749c1d05`.
