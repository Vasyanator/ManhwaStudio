# Module: src/widgets

## Purpose
Reusable egui widgets shared by the application UI. Widgets in this directory should wrap
egui primitives behind small typed APIs and keep long-running work off the GUI thread.

## Architecture
Widgets are imported through `mod.rs`, which re-exports the public widget types used by tabs,
canvas UI, launcher pages, and isolated test binaries. Stateful widgets keep only UI state and
delegate durable project or runtime state to callers.

Wheel-aware controls share `wheel_input_guard.rs` so open combo-box popups do not leak wheel
events or hover state into sliders and spin boxes underneath them. `SpellcheckedTextEdit` is the
exception to the narrow-state rule: it owns a process-wide spellcheck service, but dictionary
loading, word checks, and dictionary writes still run off the GUI thread.

## Files and submodules
- `mod.rs`: public export surface for reusable widgets.
- `ai_button.rs`: AI-tool button gating itself on the process-global AI capabilities
  (`ai_backend_capabilities`: backend/torch/onnxruntime) via `AiCaps::current()`. The
  optional marker badge is painter-only and must never allocate a second interactive
  rect (that would carve a hole in the button hitbox).
- `text_edit_plus.rs`: multiline text editor with per-range text color and ordered rounded
  background highlights.
- `spellchecked_line.rs`: multiline text editor with asynchronous Hunspell-compatible
  spellchecking, misspelling underlines, and global/project custom-word helpers.
- `autocomplete_line.rs`: single-line text input with inline completion and a popup suggestion
  list.
- `editable_combo_box.rs`: editable combo box combining free text input and predefined values.
- `viewport_color_selector.rs`: color selector with viewport eyedropper support.
- `wheel_combo_box.rs`, `wheel_slider.rs`, `wheel_spin_box.rs`: input widgets that consume
  mouse-wheel changes without scrolling parent views.
- `wheel_input_guard.rs`: shared popup/wheel guard used by wheel-aware widgets.
- `seed_spin_box.rs`: seed value input with random generation support.

## Contracts and invariants
- Widget drawing must not perform blocking file, network, build, model, or parsing work on the
  GUI thread.
- Public APIs should use typed inputs such as ranges, colors, ids, and response structs rather
  than relying on label parsing.
- Widgets that wrap `TextEdit` should preserve normal egui editing behavior unless their public
  contract explicitly changes it.
- Custom painting must account for wrapped and explicit newline rows without panicking on empty
  text, invalid ranges, or non-ASCII input.
- Wheel-aware widgets must consume only the intended wheel events and must not leave parent
  scroll areas permanently blocked.
- `ViewportColorSelector` samples only egui screenshot events for its own token; callers own the
  selected color and any durable persistence.

## Editing map
- To add a reusable widget, create a focused source file and re-export its public type in
  `mod.rs`.
- To change spellcheck behavior, edit `spellchecked_line.rs`.
- To change wheel consumption behavior, edit `wheel_input_guard.rs` and the specific `wheel_*`
  wrapper.
- To change text styling/highlight layout, edit `text_edit_plus.rs` and verify wrapped lines and
  explicit newlines.
- To change viewport eyedropper behavior, edit `viewport_color_selector.rs`.
