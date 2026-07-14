# `egui-docs/api/` — generated API index

GENERATED DIRECTORY — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`.

Everything here is extracted from rustdoc JSON built from the exact crate
sources in the local cargo registry. No part of it is written from memory, so
it is the authoritative answer to "does this API exist in our version?".

## Rule

**A name that does not appear in `symbols.txt` does not exist in the version we
depend on.** Grep before you write.

```bash
grep -n 'SidePanel'      egui-docs/api/symbols.txt   # no hits -> it does not exist
grep -n 'Panel::top'     egui-docs/api/symbols.txt   # -> egui::Panel::top  method  egui-0.35.0/src/containers/panel.rs:238
grep -rn 'fn rect_stroke' egui-docs/api/epaint.md    # -> exact 0.35 signature
```

## Contents

| Crate | Version | Index | Items |
|---|---|---|---|
| `egui` | 0.35.0 | [`egui.md`](egui.md) | 363 |
| `eframe` | 0.35.0 | [`eframe.md`](eframe.md) | 23 |
| `epaint` | 0.35.0 | [`epaint.md`](epaint.md) | 102 |
| `emath` | 0.35.0 | [`emath.md`](emath.md) | 59 |
| `ecolor` | 0.35.0 | [`ecolor.md`](ecolor.md) | 15 |
| `egui_extras` | 0.35.0 | [`egui_extras.md`](egui_extras.md) | 13 |

- `symbols.txt` — flat `path <TAB> kind <TAB> source location` list across all
  crates above. One line per public item and per inherent method. This is the
  fastest existence check.
- `<crate>.md` — per-crate index grouped by module: signatures, public fields,
  enum variants, inherent methods, implemented traits, and the first line of
  each doc comment, each with a `file:line` citation into the crate source.

## What this does NOT cover

Prose, rationale, project conventions, and migration traps live in the
hand-written pages one level up (`egui-docs/00-version-map.md` and friends).
This directory is a dictionary, not a guide.
