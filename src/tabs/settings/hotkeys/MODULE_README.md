# Module: src/tabs/settings/hotkeys

## Purpose
The Settings › Hotkeys pane: the user's view of the unified input model (`ms_input`). Lists every
declared action (commands, held modes, axes, pointer gestures, read-only fixed actions) grouped by
scope, lets the user rebind / remove / add / reset any number of alternatives per action, records
new bindings (keys, modifiers, mouse buttons, key sequences), edits axis and gesture details,
shows conflicts and shadowing, and persists every change to `user_config` -> `Hotkeys`.

## Architecture
`HotkeysPane` (in `mod.rs`) is a field of `SettingsTabState`; the settings tab calls
`HotkeysPane::draw(ui, &mut InputRouter)` with the studio's router. The router owns the
`InputRegistry` (declarations + current bindings + conflict report); the pane never keeps a copy.

One frame: cancel a stale capture -> show writer failures -> capture panel (`capture.rs`, needs
the router mutably) -> search field -> READ-ONLY pass over `router.registry()` (conflict summary
`conflicts.rs`, grouped list `rows.rs`) that turns clicks into `Intent`s -> the intents are
applied with `registry_mut()` (copy-on-write; effective from the next router frame) and every
resulting `OverrideEntry` is queued on the writer (`writer.rs`).

Pure rules (grouping, ordering, modifier chips, templates, conflict-row mapping, search) live in
`model.rs` and are unit-tested in `tests.rs`.

## Files and submodules
- `mod.rs`: `HotkeysPane` (`new`, `draw`, `cancel_capture`), the `Intent` application, error
  lines and the user-facing texts of `BindingError` / `CaptureError` / write failures.
- `model.rs`: `Group` / `layout_rows` (session, view, canvas, shared tool contexts, one block per
  tab with its sub-contexts and windows), `offered_modes` / `template_modes` /
  `apply_modifier_choice` ("use as modifier" chips), `template_axis_source` /
  `template_gesture_kind`, `conflicts_by_alternative` / `shadows_by_alternative`,
  `matches_query`.
- `rows.rs`: the collapsible group blocks, section headings (declared section keys), the action
  block (title, kind badge, alternatives with Rebind / Remove, axis source / sensitivity / invert,
  gesture kind and touch source, conflicts in the error colour, shadows as weak info, Add / Add
  without keys / Add touch gesture, Reset), fixed actions read-only, jump scroll + highlight.
- `capture.rs`: the capture panel (live recorded and held chords, the mouse capture zone, the
  modifier chips, Done / Cancel) and the finish step.
- `conflicts.rs`: the "All conflicts (N)" summary with Show (jump) buttons and the truncated
  analysis lines.
- `writer.rs`: `OverrideWriter`, one lazily spawned worker that writes entries in order
  (coalesced per id per batch) through `ms_input::write_override`, failures reported back.
- `tests.rs`: unit tests of `model.rs` over in-memory registries.

## Contracts and invariants
- No file I/O on the GUI thread: every change goes through `OverrideWriter::submit`. The worker
  is single so two writes of one id never land out of order; its spawn result is logged and a
  failure is shown in the pane.
- Unlimited alternatives in the UI; the model's corruption guard (`MAX_ALTERNATIVES`) surfaces as
  an error line, never as a hidden cap.
- Capture (plan P6): mouse buttons are recorded only inside the zone rect handed to
  `capture_set_mouse_zone` every frame the panel is drawn, so Done / Cancel clicks are never
  recorded; Esc is a bindable key, the capture ends only by Done / Cancel (or, for modes, axes and
  gestures, automatically after the first chord). A running capture suppresses every binding and
  eats keys: the pane cancels a capture that survived a frame in which the pane was not drawn,
  and `cancel_capture` lets the owner cancel it when the section is left.
- Mode references are structural (P3): a capture keeps the template's references unless the user
  changes the chips; a selected mode's chord pressed together with the key is stripped from the
  captured buttons and stored as the reference.
- Conflicts are per alternative (`conflict_text` / `shadow_text` from `ms_input`); shadows are
  information, not errors.
- Every user-visible text is a `settings.hotkeys.*` key (en + ru); binding and button names come
  from `ms_input` labels. Localized collapsing headers carry stable `id_salt`s.
- Project widgets only: `WheelSpinBox` (sensitivity), `SearchableComboBox` (touch sources); no
  egui `ComboBox` / `Slider` / `DragValue`.

## Editing map
- New grouping or ordering rule: `model.rs` (`Group`, `layout_rows`) + `tests.rs`.
- New per-alternative editor (a new `Binding` field): `rows.rs`.
- Capture UX: `capture.rs`; capture recording itself lives in `ms_input::capture`.
- Persistence behaviour: `writer.rs`; the on-disk format is owned by `ms_input::persist`.
