# Seeing the running UI: the egui inspection protocol

You do not have to reason about this app's UI from source alone. egui 0.35 ships an
**inspection protocol**, and this repo is already wired for it: an agent can attach to the
live process, read the widget tree, synthesize input, and take screenshots.

## 1. What it is

`eframe` 0.35 has an opt-in `inspection` feature that pulls in the `egui_inspection` crate:

```toml
# eframe-0.35.0/Cargo.toml:76-79
inspection = [
    "dep:egui_inspection",
    "accesskit",
]
```

`egui_inspection` binds a TCP listener and serves the widget tree + an input sink
(`egui_inspection-0.35.0/src/plugin.rs:295`). It is armed by an env var and defaults to a
fixed address:

```rust
// egui_inspection-0.35.0/src/lib.rs:18
pub const INSPECTION_ENV_VAR: &str = "EGUI_INSPECTION";
// egui_inspection-0.35.0/src/lib.rs:24
pub const DEFAULT_INSPECTION_ADDR: &str = "127.0.0.1:5719";
```

The app exposes it behind its own cargo feature, off by default so release builds are
unaffected (`Cargo.toml:28-32`):

```toml
# Dev/debug: enable egui 0.35's inspection protocol so an external inspector
# (e.g. the `egui_mcp` MCP server) can attach over TCP …
inspection = ["eframe/inspection"]
```

## 2. How to launch

Use the bundled script — it builds with the feature, sets the env var, passes `--no-ai` (no
backend/network needed to inspect UI), and polls until the port is listening
(`.claude/skills/egui-mcp/launch.sh:24,41,43,48`):

```bash
bash .claude/skills/egui-mcp/launch.sh /path/to/chapter   # chapter dir optional
# -> "READY: inspection listening on 127.0.0.1:5719"
```

Equivalent by hand: `EGUI_INSPECTION=1 cargo run --features inspection -- --no-ai`. Port is
overridable via `EGUI_INSPECTION_PORT` (`launch.sh:17,24`).

**Run it with the Bash sandbox disabled.** The GUI binary opens X11/GPU and binds a TCP port;
the sandbox kills it (signal 16 → exit 144, *empty* output — even the script's own `echo`s are
swallowed). See `.claude/skills/egui-mcp/SKILL.md` ("Launch needs the sandbox OFF").

## 3. Driving it

The `egui` MCP server (binary `egui-mcp`) bridges the protocol to MCP tools:
`attach`/`status`/`disconnect`, `query_tree`, `get_node`, `screenshot`, `click`, `hover`,
`drag`, `scroll`, `type_text`, `press_key`, `wait_for`, `resize`, `batch`.

The loop is **observe → act → verify**:

1. `attach` (127.0.0.1:5719) — always first.
2. `query_tree` and/or `screenshot` to orient.
3. Act with a **locator** (a node `id` from `query_tree`, a `role`, or `content_contains`) —
   raw `pos` only as a last resort.
4. Verify: `query_tree` for the new state, `screenshot`, or `wait_for` to poll async/animated
   UI. `batch` does act+observe in one round trip.

Everything is one logical-point coordinate frame: a node's `bounds` center is exactly where to
`click`, and a default screenshot pixel is a logical point.

**Full tool table, the fallback stdio client (`mcp_drive.py`), and the field caveats live in
`.claude/skills/egui-mcp/SKILL.md` — read it, do not re-derive it.** The caveats that bite
most: custom-painted canvas content is nearly invisible to `query_tree` (screenshot and click
by `pos`); synthetic Ctrl+wheel does not reproduce canvas zoom; child-viewport modals do not
receive `click` on their buttons (close them with `Escape`).

## 4. The accessibility tree: label vs value

`query_tree` walks the AccessKit tree egui emits from each widget's `WidgetInfo`
(`egui-0.35.0/src/data/output.rs:538`). The mapping to AccessKit fields has one trap:

```rust
// egui-0.35.0/src/response.rs:942-947
if let Some(label) = info.label {
    if matches!(builder.role(), Role::Label) {
        builder.set_value(label);
    } else {
        builder.set_label(label);
    }
}
```

So a **`Label`'s text lands in `value`, not in `label`** (`WidgetType::Label => Role::Label`,
`response.rs:916`), while a `Button`'s caption lands in `label`. Text-edit contents also go to
`value` (`response.rs:950`, from `info.current_text_value`).

Practical rule for agents: **match with `content_contains`**, which checks both fields.
`label_contains` alone silently misses every `Label`, monospace readout, and counter.

A widget is only visible to `query_tree` if it produced a `WidgetInfo` — i.e. if it went
through a real `Response`. Anything drawn straight with `ui.painter()` (the image canvas, text
overlays, the `marked_scroll` bar decorations, the `AiButton` marker badge) has **no node**.
For those, screenshot and act by `pos`.

## 5. There are NO UI tests in this repo

State this plainly because it is easy to assume otherwise: **`egui_kittest` is not a
dependency and there is no headless UI test anywhere in the tree.** It is only *proposed*, in
`docs/tutorial-plan.md:35` ("currently absent; add `egui_kittest = "0.35"` under a new
`[dev-dependencies]`"), scheduled in the phasing at `docs/tutorial-plan.md:112`, and still an
open decision at `docs/tutorial-plan.md:122`.

Consequences:

- Do not claim, cite, or "re-run" a UI test suite. It does not exist.
- Verification of a UI change is **manual or MCP-driven** (§3), not automated.
- If someone lands `egui_kittest` and a first headless test, **this page must be updated** —
  the "no UI tests" statement becomes false and the verification advice changes.

## Editing map

- To change how the app is launched inspection-ready: `.claude/skills/egui-mcp/launch.sh`.
- To change the agent-facing driving protocol/caveats: `.claude/skills/egui-mcp/SKILL.md`
  (the single source of truth; this page only points at it).
- To turn the feature on/off in builds: `Cargo.toml:28-32` (`inspection = ["eframe/inspection"]`).
- To make a widget visible to `query_tree`: give it a real `Response`/`WidgetInfo` instead of
  raw painter output; see `egui-0.35.0/src/response.rs:900-960` for the field mapping.
- If UI tests ever land: update §5 here and `04-widgets.md`'s verification guidance.
