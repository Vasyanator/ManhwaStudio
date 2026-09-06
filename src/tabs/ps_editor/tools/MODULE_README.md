# Module: src/tabs/ps_editor/tools

## Purpose
Tool subsystem for the PS-like editor. Defines the `PsTool` trait and the concrete tools the tab
routes pointer input to. The tab owns a `Vec<Box<dyn PsTool>>`, so adding a tool does not touch the
tab orchestration beyond registration.

## Architecture
`mod.rs` defines the contract:
- `PsToolId`: stable tool identity (toolbar selection + hotkeys).
- `PsToolSection`: toolbar grouping (`Brushes` / `Selection` / `Manipulation`). `PsToolSection::ORDER`
  is the toolbar's top-to-bottom order and `PsToolId::section` the assignment, so the toolbar renders
  headings without a hard-coded tool list; a new tool only picks a section.
- `PsToolContext`: per-frame mutable access to the active `LayerStack` and page `Selection`, plus
  resolved pointer/button state in image pixel coordinates, the frame's `egui::Modifiers`, and the
  two gesture-control keys (`cancel_pressed` = Esc, `remove_point_pressed` = Backspace/Delete).
- `ToolOutcome`: what changed (image-space `DirtyRect` for tile invalidation, `selection_changed`).
- `PsHotkeyRow`: one `(action, keys)` row of the tab's «Горячие клавиши» panel. Both halves are
  ALREADY LOCALIZED by the tool that built the row, so a language-dependent key name stays owned by
  the tool; the panel only lays the pair out as a two-column grid.
- `PsTool`: `interact` (one frame of input), `draw_overlay` (screen-space cursor/preview),
  `has_options` + `options_ui` (the «Выбранный инструмент» panel's pair), `hotkey_rows` (the tool's
  own shortcut inventory), `gesture_in_flight` (REQUIRED, see below), the two gesture-lifecycle
  hooks `reset` / `freeze` (both default to a no-op), and the `as_brush_mut` downcast hook used by
  the tab to forward brush wheel/size gestures without a full `Any` downcast.

Tools never touch GPU textures, files, shared models, or the backend. They mutate the in-memory
stack/selection only; the tab converts `ToolOutcome::dirty` into `TiledTexture` re-uploads.

**A gesture may not START outside the canvas viewport, but one already in flight may leave it.**
Since the tab's panels FLOAT over the canvas, `PsToolContext::pointer_in_viewport` is false wherever
a dock panel covers the pointer, and a tool that only checks the press frame paints under a panel:
the tab already suppresses `primary_pressed` off-viewport, so such a check is dead code and the
gesture starts anyway. Gate on the tool's own in-flight marker instead (`BrushTool::last_world`),
which blocks a start on EVERY frame while letting a stroke crossing a panel continue — the same rule
the tab applies to panning.

The tool's CURSOR PREVIEW obeys the same rule, and `PsTool::gesture_in_flight` is how it does.
`draw_overlay`'s pointer is dropped by the tab (`../mod.rs`, `overlay_pointer`) whenever the pointer
is occluded AND no gesture is running: a brush circle that keeps tracking under a panel advertises a
stroke the tools already refuse to start there. `gesture_in_flight` is REQUIRED with no default body
(like `hotkey_rows` and `has_options`) because only the tool knows what an accepted gesture is:
`BrushTool::last_world`, `SelectTool::gesture`, `TransformTool`/`DeformTool::drag` — never a per-frame
overlay cache (`gizmo`, `handles`) and never `SelectTool::last_pointer`, which outlives a commit.
Two answers must not be given: `true` for a press the tool itself REFUSED (the button is held there
too, which is why the tab must not substitute `primary_down` for this method — that is the exact bug
it was added to fix), and `false` for the lasso's PENDING POLYGON, which is a live gesture with the
button up and would otherwise lose its rubber band every time the cursor crossed a panel.

## Shortcut inventory (`hotkey_rows`) — required, never defaulted
`hotkey_rows` has NO default body on purpose. A tool is the only place that knows which keys and
mouse gestures it interprets, so a new tool is forced by the compiler to state them; a defaulted
empty list would silently ship an undocumented tool, and that is exactly the failure this method
exists to prevent. Returning an empty `Vec` is the explicit way to say "this tool has none".

**Hint text lives in `hotkey_rows`, never in `options_ui`.** `options_ui` is for controls that
change a parameter (the brush colour/radius/erase, the selection `mode_row`); a `ui.label` that
merely describes a key or a drag belongs in a row instead, because the panel renders rows as a
two-column grid and a sentence like "Shift adds, Alt subtracts" cannot be laid out there. Split
such a sentence into one row per shortcut. Both halves of a row must be non-empty — the unit tests
in `mod.rs` assert it against the reference (`en`) catalog, so an empty translation there fails
that test. A missing translation in another catalog falls back to `en` and is not caught here.

Rows are rebuilt on every call, so a runtime language switch is reflected without any invalidation.

## Parameters (`has_options` / `options_ui`) — the pair, and which half is required
`has_options` is REQUIRED with no default body, for the same reason as `hotkey_rows`: only the tool
knows whether it owns a parameter, so a new tool is forced by the compiler to answer. It is the
«Выбранный инструмент» panel's ONLY question — a `false` makes the panel print
`ps_editor.active_tool.no_options` and never call `options_ui`.

`options_ui` therefore DEFAULTS to a no-op, and a tool answering `false` does not implement it at
all. `TransformTool` and `DeformTool` are exactly that case: they have no parameters, only
positional gizmo drags, and those are `hotkey_rows`. `BrushTool` (colour / radius / eraser) and
`SelectTool` (the persistent combination mode) answer `true`.

Answer `true` only for a control that CHANGES a parameter. A label describing a key or a drag is a
row, not an option — see the section above.

Scope boundary: a tool lists only keys it handles ITSELF. Tab-level shortcuts (the B/M/L/V tool
letters, Ctrl+D, undo/redo, pan, zoom) belong to `PsEditorTabState::handle_hotkeys` in `../mod.rs`
and are appended by the panel, not by a tool.

## Gesture lifecycle (`reset` / `freeze`)
A gesture can span many frames and can end with the button UP (the pending lasso polygon), so the
tab — not the tool — owns the two transitions the tool cannot observe:
- **`reset`**: abandon the in-progress gesture; the tool must be usable again immediately, and the
  committed page selection must be left exactly as it was. Called on a tool switch (the OUTGOING
  tool, from `PsEditorTabState::set_active_tool`, the single writer of `active_tool_idx`), on a page
  switch (`request_page`), and on Esc pressed while input is suppressed. Without it a pending
  outline commits minutes later, with a stale combination mode and — across a page switch — the
  previous page's coordinates.
- **`freeze`**: called once per frame whose canvas input went to a pan or a text-layer drag instead
  of the tool. Those frames hide the button events, so the next routed frame can see "button up, no
  release" and mistake a swallowed release for an interrupted drag. `SelectTool` records the
  suppression and commits on resume instead of aborting; the abort branch stays reachable for the
  real case (focus loss, another widget grabbing the button). Tools whose gesture survives a
  suppressed frame unharmed keep the default no-op, so panning does not change their behavior.

`BrushTool::reset` drops the stroke's last point (otherwise the next stamp draws a segment from
another page); `TransformTool` / `DeformTool` drop their control drag and per-frame caches.

## Files and submodules
- `mod.rs`: trait, context, outcome, and `PsToolId`.
- `brush.rs`: `BrushTool` — round color brush on the active editable raster layer, clipped by the
  selection. Reuses `crate::tools::MaskBrush` for radius gesture/size shortcuts and cursor sizing.
- `select.rs`: `SelectTool` — rectangle marquee or freehand/polygonal lasso (one struct,
  `SelectMode`), building the page `Selection` with a Photoshop combination mode.
- `transform.rs`: `TransformTool` — move / rotate / uniform scale of the active raster layer's
  `LayerTransform` (no pixels, so no tile re-upload); base and deformed layers are refused.
- `deform.rs`: `DeformTool` — grid-point drag over the active raster's `deform` mesh (page px),
  initializing an identity grid on first use.

## Selection gesture model (`select.rs`)
- **Combination mode.** `SelectionOp` (`super::selection`) decides how a finished shape meets the
  existing mask: `Replace` / `Add` / `Subtract` / `Intersect`. `base_op` is the persistent options-row
  choice; `gesture_op(base, mods)` overrides it per gesture — Shift = add, Alt = subtract,
  Shift+Alt = intersect. It is sampled **once, on the press frame**, and never re-read: Alt held
  BEFORE the press means "subtract", Alt held AFTER it means "straight segment", and only a
  press-time sample can tell the two apart. The two meanings must not overlap, so `Gesture::alt_armed`
  gates the second one: Alt already down at the press stays the mode latch until it is RELEASED once
  inside the gesture. Without that gate an Alt-drag subtracts in polyline steps instead of freehand —
  the one place Photoshop's own key assignment is ambiguous.
- **State machine.** `SelectTool::step(GestureInput) -> GestureAction` is pure (no `PsToolContext`,
  no `Ui`, no `Selection`) and owns the whole lifecycle; `interact` is a thin adapter that performs
  the commit. States: idle → dragging (freehand sampling every `FREEHAND_MIN_STEP` image px, skipped
  while Alt is held AND armed) → either finished, or **pending polygon** (release with Alt: an anchor is
  committed and the outline stays alive with the button UP; a further press adds an anchor, releasing
  Alt closes the path). Esc aborts without touching the selection; Backspace/Delete pops the last
  lasso vertex and aborts at zero. `Gesture::rubber_band` is recomputed every frame because
  `draw_overlay` receives no modifiers.
- **A click is not a special case.** A degenerate rect / a sub-3-point polygon is committed like any
  other shape: `Replace` clears the mask (click to deselect), the other ops leave it alone. Do not
  add an early return for it.

## Contracts and invariants
- The brush only paints when `LayerStack::active_editable_mut()` returns a layer (locked base
  layers are skipped). Erasing writes transparent pixels; painting replaces with the brush color.
- Selection-clip: when a selection is active, pixels outside it are left untouched.
- **Empty means `None`.** `ensure_selection` allocates a page-sized mask eagerly, so every commit
  path must end in `PsToolContext::normalize_selection` (pure core: `normalize_selection_slot`),
  which drops an all-zero mask. A `Some(all-zero)` selection is invisible (`!any()` hides the
  marquee) yet still blocks the brush, which only tests `Option::is_some`. The rule holds for the
  tab too: every store into `PsEditorTabState::selection` goes through its `set_selection`.
- **A gesture never outlives its context.** `reset`/`freeze` above are part of the contract, not an
  optimization: a tool that keeps cross-frame state must implement them.
- `interact` must report the modified region via `ToolOutcome::dirty` so only affected tiles
  re-upload; reporting too small a rect leaves stale pixels on screen.
- Coordinates in `PsToolContext::pointer_image` are image pixels (fractional); tools round as
  needed. `draw_overlay` receives the `ViewTransform` to map image→screen.

## Editing map
- To change when the tool's cursor preview is hidden, edit `overlay_pointer` in `../mod.rs` (the
  pure decision, unit-tested there) and the tools' `gesture_in_flight`.
- To add a tool: implement `PsTool` (including the REQUIRED `hotkey_rows` and `gesture_in_flight`),
  add a `PsToolId`, give
  it a `PsToolSection`, register it in `PsEditorTabState::default`, and add it to the
  `registered_tools` fixture in `mod.rs`'s tests. Add a hotkey in
  `PsEditorTabState::handle_hotkeys` if wanted.
- To change what the «Горячие клавиши» panel shows for a tool, edit that tool's `hotkey_rows` —
  never `options_ui`.
- To change brush behavior (color, size, erase, clipping), edit `brush.rs`.
- To change selection shapes or mask combination, edit `select.rs` and `super::selection`.
- To change how a gesture starts/ends (keys, Alt semantics, pending polygon, the frozen-resume
  branch), edit `SelectTool::step` — it is the single decision point, and its unit tests live
  beside it.
- To change when a gesture is abandoned or frozen, edit `PsEditorTabState::set_active_tool` /
  `reset_active_tool` and the canvas routing gate in `../mod.rs`, plus the tools' `reset`/`freeze`.
- The selection-mode row is a label plus a WRAPPING button row (`ui.horizontal_wrapped`): the tool
  panel is narrow and the four localized mode names overflow a single line in several languages.
