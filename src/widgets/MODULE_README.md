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
  rect (that would carve a hole in the button hitbox). Because it is painted OUTSIDE
  the button's rect, a caller budgeting a button's width asks `marker_badge_overhang`
  rather than re-deriving the pill's geometry.
- `text_edit_plus.rs`: multiline text editor with per-range text color and ordered rounded
  background highlights.
- `spellchecked_line.rs`: text editor with asynchronous Hunspell-compatible
  spellchecking, misspelling underlines, and global/project custom-word helpers. Two constructors
  select the underlying `egui::TextEdit`: `multiline` (grows with the text; `desired_rows` only sets
  the initial height) and `singleline` (exactly one row, `Enter` does not insert a newline,
  `desired_rows` ignored). Everything else — the spellcheck layouter, the builder methods, and the
  `TextEditOutput` contract — is shared, so a caller that needs a genuinely one-line value must pick
  `singleline` rather than `multiline().desired_rows(1)`.
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
- `font_preview.rs`: shared egui font-registration helpers for own-typeface font previews
  (`combo_font_family_name`, `is_font_family_bound`, `request_font_family`). Deterministic
  `(font identity, content hash, face_index)` → family naming plus the one-time registration
  of a font into egui. `request_font_family` returns a `PreviewFontFamily`
  (`Ready` / `Pending` / `Unavailable`): the file READ is queued on worker threads and only
  the `add_font` call happens on the GUI thread, so a caller draws its fallback and is
  repainted when the family binds. The path is only the byte source: two entries
  can share a FILE and still be different fonts (the bundled `fonts/ui` entry and a user
  import of it), and moving a file must not re-register it. The CONTENT HASH
  (`FontEntry::content_hash`) is in the key because egui never re-reads a registered font:
  without it, replacing the file behind one PostScript name would keep the UI drawing the
  old typeface while the renderer drew the new one. `0` is the documented "content unknown"
  sentinel (bundled stack, unreadable file, the system-font picker catalog), and those
  entries share one family per `(identity, face)` as before. Used by the typing create/edit
  panels and the settings font-settings widget. Registration is ADD-ONLY (egui never evicts
  a font), so a caller scrolling a large catalog must bound how many distinct families it
  registers.
- `hangul_keyboard.rs`: on-screen Korean jamo keyboard. `HangulKeyboardState` holds the latched
  choseong/jungseong/jongseong indices, the mode, and the user-selected insert placement
  (`HangulInsertPlacement`: `Append` / `ReplacePrevious`);
  `show_hangul_keyboard(ui, &mut state) -> HangulKeyboardOutcome` draws the CONTENT only — the
  consumer owns the window/area, the target field, and the caret. The widget never mutates text and
  never touches `egui::TextEditState`; it reports `insert` / `replace_previous` and at
  most one insert per frame. `Compose` mode latches exactly one key per row (T index 0 is the
  explicit "no final consonant" key and is stored as `None`, never `Some(0)`) and inserts the
  composed syllable on the explicit `Insert` button. An explicit checkbox toggle in the action row
  chooses the placement: append a new syllable or replace the character before the caret (the
  consumer computes the replaced range live from the caret; the widget only reports the choice).
  `load_syllable` pre-latches from a syllable and presets the placement to `ReplacePrevious`.
  `Direct` mode makes the same keys momentary and inserts a single compatibility jamo per click
  (always appending), plus a quick row of frequent onomatopoeia jamo.
  All jamo arithmetic and caption tables come from `ms_text_util::hangul`. The jamo captions and the
  `∅` marker are Unicode data, deliberately not localized (`dev-docs/i18n_exclusions.md`).
- `panel_dock/`: the dockable-panel system (design contract:
  `dev-docs/dockable_panels_plan.md`). It is a subsystem, not a single widget, and has its own
  `MODULE_README.md`. Pure layer: `model.rs` holds the panel/tab graph (`DockLayout`, `PanelNode`,
  `PanelAnchor`) with its invariants — mutated only through its checked operations, never through a
  handed-out `&mut PanelNode` — and `solver.rs` resolves that graph into rects (`solve` →
  `SolvedLayout`) preserving `DOCK_GAP`, shrinking on both axes exactly the panels whose size places
  an overflowing edge — the ones the user resized by hand LAST — and translating what is left over.
  Neither file may touch `egui::Context`, `Ui` or `Memory`.
  Widget layer: `PanelTab` DECLARES one tab per frame (title, visibility, min/initial size, body),
  `CollapsiblePanel` draws one panel, and the `PanelDock` frame driver (`begin` →
  `tab(..).show(..)` → `end`) queues every body and runs them in panel order — which is what lets
  two tabs borrow `&mut` of two different fields of the caller. Its `PanelDockState` must live in
  its own caller field, disjoint from everything the bodies touch. A panel is as big as its LARGEST
  tab, its body fills that size and scrolls both axes, and what it reports back is the CONTENT's
  size — never the drawn one. Gesture layer: `drag.rs` decides
  where a dragged panel would dock (`find_snap`), keeps two panels out of one slot
  (`resolve_slot`), and answers where a dropped tab lands; the widgets only REPORT the gestures and
  the driver applies them through the model. Persistence layer: `persist.rs` owns the
  self-versioned `PanelLayout` section of `user_config.json` and `PanelLayoutWriter`, its handle on
  the shared `config_saver::ConfigSaver` writer thread, which the application feeds from
  `PanelDockState::take_dirty_layouts`; it is the only file of the subsystem that reaches the disk,
  and the debounce/retry policy behind it lives in `src/config_saver.rs`. Sub-windows are a later phase.
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
  Optional action mode: `with_action(label)` adds a clickable button below the tooltip content
  (the `on_hover_ui` tooltip is interactive in egui 0.35, so it stays open while the pointer moves
  onto the button). Use `show_with_action`, which returns `HelpHintResponse { response, action_clicked }`;
  plain `show` still renders the button but discards its click. Callers pass already-localized labels.

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
- Every floating panel of the studio must be built from `CollapsiblePanel` + `PanelTab` — no
  hand-rolled `Area + Frame::popup` panels and no bare `egui::Window` used as a panel. (Migration of
  the existing surfaces is phased; new panels have no exemption.) Its layout solver is a pure
  function: it keeps no egui state, does no I/O and no logging, and returns the same rects for the
  same inputs. Dock panels stay on `egui::Order::Foreground`, because canvas input gating is z-order
  based.
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
- To change jamo keyboard layout or latch semantics, edit `hangul_keyboard.rs`; the syllable
  arithmetic and the compatibility-jamo tables belong to `crates/ms-text-util/src/hangul.rs`.
- To change panel docking (arrangement rules, gaps, shrinking, or later the panel widgets), edit
  `panel_dock/` and read its own `MODULE_README.md` first.
