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
  The active dictionary follows the TYPESETTING language (`ms_text_util::language::text_language`,
  like hyphenation and font coverage), never the UI language. `dictionary_spec(language)` is the
  language→dictionary provenance table (on-disk stem + verified `.aff`/`.dic` URLs); it is pure,
  total, and unit-tested. Most dictionaries come from `LibreOffice/dictionaries`, but `fr`, `pl`,
  and `sl` come from `wooorm/dictionaries` on purpose (LibreOffice has no `fr_FR` directory, and its
  `pl_PL`/`sl_SI` files are `SET ISO8859-2`, which this module's UTF-8 `read_to_string` load path
  rejects — do not "fix" these back). The background worker compares `text_language()` each batch
  and downloads the active language's dictionary at most once per language
  (`download_attempted: HashSet<TextLanguage>`), so a failed download of one language never blocks
  another. Per-word matching is language-first, script-second: a word in the active language's own
  script is judged ONLY by that language's dictionary (a stale same-script dictionary left on disk,
  e.g. `uk_UA` after switching to Russian, must not vote), while a word of the other script is judged
  by any dictionary of that script so mixed-script text keeps working. If the active language's
  dictionary is missing, its words are left unmarked rather than judged by a sibling dictionary that
  would flag nearly all of them. The per-word cache key carries the typesetting language, and the
  whole cache is cleared whenever the loaded dictionary set changes, so a verdict from one language
  never survives a switch. On wasm the download layer is unavailable; the word is left unmarked.
- `autocomplete_line.rs`: single-line text input with inline completion and a popup suggestion
  list.
- `editable_combo_box.rs`: editable combo box combining free text input and predefined values.
- `viewport_color_selector.rs`: color selector with viewport eyedropper support.
- `wheel_combo_box.rs`, `wheel_slider.rs`, `wheel_spin_box.rs`: input widgets that consume
  mouse-wheel changes without scrolling parent views.
- `wheel_input_guard.rs`: shared popup/wheel guard used by wheel-aware widgets.
- `seed_spin_box.rs`: seed value input with random generation support.
- `help_hint.rs`: light-gray circled "?" icon whose hover tooltip explains a control. The
  tooltip may carry a localized text line, an animated WebP hint from the `ms-gifs` crate, or
  both — text first, animation below it — selected by the constructors (`animated`, `text`,
  `with_text`, `with_animation`); callers pass already-localized text. The text line wraps at
  320 pt in a width-capped child ui, so it never stretches the tooltip out to the animation's
  full width, and a short line still leaves the tooltip narrow. The animation is rendered 1:1
  (texel = point) and only scaled down, uniformly, when it exceeds 500x400 pt — never
  stretched up to the tooltip width. A hint with no animation never reaches the playback cache
  and never starts a worker, so the text-only mode is independent of `ms-gifs`; a hint whose
  animation is blacklisted still shows its text, and the tooltip is dropped only when there is
  neither text nor a usable animation. Playback streams one frame at a time
  on a background `ms_thread` worker through two reusable RGBA buffers, so CPU memory is one
  compositing canvas plus publication buffers (about 1.6 MB each for the largest asset) and is
  independent of frame count. The GUI uploads the latest ready frame into one reused
  `TextureHandle`. A process-wide single slot stops the previous worker and drops its texture
  when another hint is hovered; a tooltip-body heartbeat stops the worker when the tooltip is
  no longer shown. The worker slot is released through an RAII guard, so a panic cannot wedge
  playback. A hint whose open or frame decode fails is logged once and blacklisted for the session.

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
- `WheelComboBox::from_label` seeds the widget id from the label text. When the label is localized
  (`t!("…")`), chain `.id_salt("stable_key")` so the id stays language-independent
  (`docs/i18n_exclusions.md` §C); user-visible widget labels are localized through `ms-i18n`, but
  the id source must not follow the translation.

## Editing map
- To add a reusable widget, create a focused source file and re-export its public type in
  `mod.rs`.
- To change spellcheck behavior, edit `spellchecked_line.rs`.
- To change wheel consumption behavior, edit `wheel_input_guard.rs` and the specific `wheel_*`
  wrapper.
- To change text styling/highlight layout, edit `text_edit_plus.rs` and verify wrapped lines and
  explicit newlines.
- To change viewport eyedropper behavior, edit `viewport_color_selector.rs`.
