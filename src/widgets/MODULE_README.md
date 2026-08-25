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
  list. Matching is SEGMENT-based, not whole-line: a query segment is any suffix of the text
  before the caret that starts at a word/punctuation boundary (`-` and `'` are not boundaries —
  they occur in names). NO SEGMENT WINS THE FRAME: variants are collected from every boundary and
  merged into one ranked list, ordered from the segment NEAREST THE CARET outwards, because a
  single winner let one stale candidate matching the whole line hide the completion of the two
  letters being typed. Each `Suggestion` therefore carries its OWN `segment_start`; every consumer
  splices at the start of the variant it picked. Inside one segment the order is whole-candidate
  prefix matches before word-boundary matches; a word-boundary match inserts the candidate's TAIL
  from the matched word on (typing `Су` offers `Су Лин`), except when that tail equals the query,
  where the whole candidate is inserted so the entry still does something. Variants are
  deduplicated by the LINE they would produce, not by candidate or by inserted text, so the same
  offer reached from two segments is one entry. A variant that adds nothing is dropped outright
  (`is_useless_variant`): its insertion equals the segment it replaces, or the text just before
  the segment already spells the insertion's beginning so splicing would repeat it. A candidate of
  more than `MAX_COMPLETION_CANDIDATE_WORDS` (4) whitespace-separated parts is a descriptive row
  rather than a name: it is dropped from the word-boundary pass entirely and from the prefix pass
  unless the segment starts at offset 0 — dropped INSIDE each per-segment scan, before anything is
  collected, so it cannot crowd the ranked list. Writing into `value` is gated harder than the popup
  is (`inline_insertion_allowed`): the segment needs at least `MIN_INLINE_SIGNIFICANT_CHARS` (2)
  non-whitespace chars and exactly one surviving variant, because the candidate's spelling wins
  over the user's and a one-letter match once capitalised a conjunction irreversibly. Neither
  gate affects what the popup lists. A
  tail entry shows the full candidate dimmed beside it via a two-section `LayoutJob`. Only the
  segment range is ever replaced — the rest of the line survives, and the caret lands right after
  the insertion. The widget mutates the caller's `value` SPECULATIVELY while drawing the inline
  completion and remembers that exact string, so "is a completion standing?" is a comparison, not
  a prefix heuristic. While one stands, `Escape` restores the typed text and caret REGARDLESS of
  whether suggestions resolved this frame — the caller may rebuild its candidate list between
  frames, and a speculative value must never become unreversible; losing focus commits it instead.
  Case-insensitive matching streams `char::to_lowercase` instead of allocating lowercase copies,
  because offsets found in a lowercased copy cannot be mapped back onto the original; for the same
  reason the inline selection anchor is the number of CANDIDATE chars the query consumed, never
  the query's own char count (folding can expand one char into several).
- `editable_combo_box.rs`: editable combo box combining free text input and predefined values.
- `searchable_combo_box.rs`: combo box whose drop-down rows carry a MAIN line — optionally
  drawn in that row's own `egui::FontFamily` — and a SECOND line always drawn in the interface
  font, grey and at EXACTLY half the main line's size at every `primary_size` (no floor: one
  would make the two lines look alike as soon as the main line got small), plus an on-demand
  search field that FILTERS the list (case-insensitive substring of either line, empty query
  matches everything) and colours every occurrence. The widget draws TWO controls: the combo
  button (caption + the drop-down arrow INSIDE it) and a square magnifier button after it,
  and `width(..)` is the width of both together — the popup is that wide, the combo button
  gets what is left, and a caller budgeting a row asks `search_button_overhang(ui,
  primary_size)` for the difference instead of re-deriving it (the same contract
  `ai_button::marker_badge_overhang` has). SEARCH IS A MODE, not furniture: the popup opens as
  a plain list and the field appears only when the user types into the open popup or presses
  the magnifier — then it takes its own space ABOVE the list, which is pushed down and never
  covered. The characters that summoned it are taken out of the event queue
  (`take_typed_text`, pure and unit-tested) and seeded into the query, because nothing holds
  focus while the field is hidden and the field created later in that same frame would
  otherwise lose the first keystroke; opening the popup also drops whatever focus another
  widget held (`Memory::stop_text_input`), or the tab's own text editor would swallow it. The
  magnifier both opens (straight into search mode) and toggles, and because it sits OUTSIDE
  the popup's `Area` its click reads as "clicked elsewhere" to egui — the widget takes that
  close back. It knows nothing about fonts: the caller lends it a resolver `usize -> Option<FontFamily>` and
  owns the registration those families need (`font_preview.rs`) and the cap on how many it is
  willing to register. Where the second line goes is the caller's choice of `RowLayout`:
  `Tall` (the default, «высокий») puts it UNDER the main line, `Wide` («широкий») puts it
  AFTER the main line on the same text row, which halves the row height and is what makes a
  long catalog usable. Row height is UNIFORM across the list WITHIN one layout, and it differs
  BETWEEN layouts — that is the point of the switch. `Tall` reserves the second line's height
  for every row as soon as ANY row has one; `Wide` reserves none at all and never inspects the
  items, so its height cannot depend on what filtering leaves on screen. Everything derived
  from the height follows the layout through one `RowGeometry`: the `show_rows` pitch, the
  reveal/scroll arithmetic, and the row rect. Uniformity is load-bearing —
  `ScrollArea::show_rows` positions rows by multiplying one height by an index, so a per-row
  height would misplace every row after the first odd one, and a conditional second line would
  make the list jump as filtering changes which rows are shown; the height is pinned through
  `TextFormat::line_height` rather than trusted to the row's own face, and each row's painter
  is clipped to its rect so a tall face cannot bleed into its neighbour. Every galley the
  widget draws — each line of every row in BOTH layouts, AND the CLOSED BUTTON's caption — it
  positions ITSELF by BASELINE (`RowBaselines::measure` / `button_caption_baseline`, then
  `paint_line_on_baseline`), never by its top edge and never by its box: epaint places a
  galley's baseline at its own face's ascent, which is 15 pt for the interface font against
  24 pt for a display face at the same nominal 16 pt, and a pinned `line_height` puts all the
  surplus BELOW that baseline — so top-edge or box placement makes a catalog's
  ink float from row to row — which reads as uneven row heights — pulls a `Wide` row's
  second line off the first, and rides the button's caption up against its top edge as the
  selected font changes. A baseline is derived from the INTERFACE font's metrics at that
  line's nominal size (`row_baseline` centres the font's line box in the band and takes its
  baseline); `Tall` gives each line a band of its own (main line band, then second line band)
  and a baseline inside it, `Wide` has one text row and therefore one shared baseline, and the
  second line starts after the main line's advance width plus a gap proportional to the main
  size. Nothing about a row's height or a baseline may be taken from the item's face — the face
  decides only what the main line looks like and how wide it is; ink that overflows the row is
  clipped, never accommodated. Each line is highlighted in its own galley, so search colouring
  is identical in both layouts. The closed button is identical in both layouts too: it shows the
  selected row's MAIN line only. Its height (`button_row_height`, which is also the square
  search button's side) is the content band — caption line box or drop-down icon, whichever is
  taller — plus `Spacing::button_padding` on both edges, raised to the minimum interactive
  height; that padding is part of the height because the caption is laid out inside
  `rect.shrink2(button_padding)`, and the caption is clipped to the text area horizontally but
  to the WHOLE button rect vertically, the way a row clips to its whole row rect. The BASELINE
  RULE is what button and rows share; the padding is not. A row keeps `ROW_VERTICAL_PADDING`
  (2 pt) around its band, the button egui's own `button_padding.y` (1 pt) — the button contract
  that keeps the control in line with its neighbours in the typing panel — so at `primary_size`
  14 the caption sits 17.2 pt below the button's top against a row's 18.2 pt: within a point of
  each other, not identical. Nor does the band make a face safe. Glyph INK that rises above the
  baseline further than the band allows is clipped by the button rect exactly as a row rect
  clips it; what can overflow is the ink actually drawn, never the face's declared ascent — a
  face may claim a 1.5 em ascent and put nothing near it. The open flag is a plain `bool` the
  widget owns, never `egui::ComboBox::is_open` — see the `WheelComboBox` defect noted under
  "Contracts and invariants". Keyboard: `Escape` drops a non-empty query AND the search row, and
  closes the popup otherwise (so at most two presses take a searching user to a closed list),
  `ArrowUp`/`ArrowDown` move inside the FILTERED list, `Enter` picks; all four are consumed
  with `InputState::consume_key` INSIDE the popup body, which runs before `Popup::show`'s own
  unconditional Escape-closes-the-popup check. The matched-character colour is chosen from the
  row's actual background rather than from the theme, because `Visuals::hyperlink_color` on
  the light-theme selection fill has less contrast than the plain row text. Matching lives in
  a GUI-free private submodule and is unit-tested; it reuses
  `autocomplete_line::ignore_case_prefix_len` instead of re-deriving case folding, scans one
  CHAR at a time so overlapping occurrences are all covered ("ana" twice in "banana"), and
  merges touching hits so the ranges it emits stay ordered and disjoint. The search field, while it is shown,
  re-requests focus on every frame NO widget holds it, because any click elsewhere in the
  popup surrenders it and nothing else in the popup is focusable; a click outside the popup
  closes it and keeps the focus it took. A row may carry two OPTIONAL per-item marks, both off
  by default so a `SearchableComboItem::new`/`with_secondary` row is byte-for-byte what it was
  before they existed: `primary_color` replaces the row-state colour on the main line's
  UNMATCHED characters, and `tooltip` is an already-localized hover text (the widget never
  translates it, and an empty string counts as absent). The search highlight is passed through
  untouched (`LineColors::primary_row`), so a coloured row still shows where the query hit; the
  second line keeps `weak_text_color` regardless; and the colour reaches TEXT only — the fill
  behind the keyboard cursor's row and behind the current selection is what keeps those rows
  recognisable whatever colour an item asks for. The marks exist for the typing tab's
  font-coverage diagnostics (`tabs::typing::panel::create_presets`: warning/error colour plus
  `font_coverage_tooltip`); the widget itself knows nothing about coverage. Because the two
  fields are public, a struct-literal construction must list them — the constructors plus the
  `primary_color(..)`/`tooltip(..)` builders are the intended shape. The response reports the
  written selection (`changed`) and, separately, the row the popup COMMITTED to this frame
  (`picked`) — a click on the ALREADY selected row writes nothing and is still a deliberate
  act, which is the only thing that can pin a font on a span whose font already equals the
  shown row. Its product call site is the typing tab's font combo
  (`tabs::typing::panel::create_presets::draw_font_combo`, both panels).
- `viewport_color_selector.rs`: color selector with viewport eyedropper support. Two entry
  points share one frame: `draw` keeps the stock `ui.color_edit_button_srgba` swatch, and
  `draw_with_presets` swaps it for `ColorPresetPicker` when the caller lends a `ColorPresets`
  set. The eyedropper outranks both — while it is active the swatch is frozen and no preset
  UI is drawn, so a sampled color can never be written into a cell by accident. Because those
  are frames in which the picker is not drawn at all, every color the sampling writes — the
  per-frame preview AND the rollback that ends a cancelled sampling — is announced to the picker
  through `note_color_picked_by_user`, otherwise the picker would read a color the user
  deliberately picked as a replacement made behind its back. The eyedropper contract (own
  screenshot token, `eyedropper_active`, `primary_click_consumed_this_frame`) is unchanged by
  the preset mode.
- `color_preset_picker.rs`: the egui palette popup extended with two rows of color presets
  (`PRESET_COLUMNS` x `PRESET_ROWS` = `PRESET_COUNT` cells) and an update/cancel action row.
  `ColorPresets` is plain data: the widget reads cells and overwrites ONE of them on an
  explicit confirmation, but ownership and persistence belong to the caller, which is told to
  save by `ColorPresetPickerOutput::presets_changed`. `to_stored`/`from_stored` speak
  PREMULTIPLIED sRGBA bytes because that is `Color32`'s own representation and therefore the
  only lossless round-trip; a stored set of the wrong length is filled from `PresetDefaults`
  instead of being rejected. The widget's own state is only the targeted cell plus the color
  that cell was last synchronized with, and "has unsaved changes" is DERIVED from those two
  rather than stored — which is what makes a color the user picked outside the popup (the
  eyedropper) light the cell up without the widget observing that change. That derivation is
  only sound while the selection still describes the world, so the widget also remembers the
  color it itself last accounted for and the color the selected cell held when it was chosen,
  and drops the selection at the start of a frame when either witness disagrees: a color
  replaced by the OWNER (another text layer selected) must not mark a cell dirty, and an index
  chosen in one title must not survive into another title's set. All of that logic lives in a private
  `PresetSelection` that never touches `egui::Ui`, so the interaction is unit-tested without a
  GUI. Two egui-0.35 facts the drawing depends on: the palette takes its width from
  `Spacing::slider_width`, not from an argument, and the popup must be
  `PopupCloseBehavior::CloseOnClickOutside` or the first click on a cell closes it.
- `wheel_combo_box.rs`, `wheel_slider.rs`, `wheel_spin_box.rs`: input widgets that consume
  mouse-wheel changes without scrolling parent views.
- `wheel_input_guard.rs`: shared popup/wheel guard used by wheel-aware widgets, and the
  shared WHEEL-STEP helpers the combo boxes react through (`wheel_steps_if_hovered`,
  `cycle_wrapped_index`, and the raw per-notch delta behind them). `WheelComboBox` and
  `SearchableComboBox` both call them, which is the point: one wheel notch must move one row
  in both, and a copy per widget is how that contract drifts. One step per FRAME is reported
  however many notches arrived in it — only the sign of the raw delta is read — and
  `cycle_wrapped_index` reduces an out-of-range index into range before its arithmetic, so a
  selection the caller has not cleaned up cannot overflow it. `combo_popup_open` is the one
  helper re-exported from `mod.rs`: a wheel consumer OUTSIDE this module (a canvas reading the
  raw wheel delta, e.g. the page manager's split board) must skip its wheel reaction while a
  list is open, exactly as the wheel widgets here do.
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
- `ColorPresets` is caller-owned data. The widget never loads or saves it and must not grow a
  disk path: it reports `presets_changed` and the owner decides where the set lives. Its public
  API is total — an out-of-range cell index returns `None`/`false`, never a panic — because the
  index can outlive the set it was chosen in (switching title replaces the whole set).
- Because both the edited color and the preset set are replaced by their owner without telling
  the widget, `ColorPresetPicker` treats its selection as a claim to be re-verified every frame,
  never as durable state. Two obligations follow for anyone editing it: `draw` must run once per
  frame while the widget is shown, and any code path that changes the color while `draw` is NOT
  reached must report it with `note_color_picked_by_user` if the user picked that color, or stay
  silent if the color was substituted for them.
- Every floating panel of the studio must be built from `CollapsiblePanel` + `PanelTab` — no
  hand-rolled `Area + Frame::popup` panels and no bare `egui::Window` used as a panel. (Migration of
  the existing surfaces is phased; new panels have no exemption.) Its layout solver is a pure
  function: it keeps no egui state, does no I/O and no logging, and returns the same rects for the
  same inputs. Dock panels stay on `egui::Order::Foreground`, because canvas input gating is z-order
  based.
- A widget that opens a popup MUST publish the wheel guard while it is open
  (`publish_combo_popup_open` every frame, `publish_combo_popup_rect` from inside the popup
  body) and must not react to the wheel itself meanwhile. That duty is what stops a slider
  under an open list from being dragged by wheel events aimed at the list.
- A popup widget must derive "is my popup open?" from an id it CONTROLS.
  `egui::ComboBox::show_ui` re-salts whatever `id_salt` it is given
  (`egui-0.35.0/src/containers/combo_box.rs:232`), so `ComboBox::is_open(ctx, already_salted_id)`
  answers `false` forever — `wheel_combo_box.rs` carries that defect and survives only because
  its popup closure publishes the guard rect before the check runs. `SearchableComboBox` owns
  a plain `bool` instead; new popup widgets should do the same.
- A popup whose content can GROW must ask for its height every frame. An `egui::Area` hands
  its body last frame's content size as this frame's `max_rect`
  (`egui-0.35.0/src/containers/area.rs:610` + `:666`) and a `ScrollArea` can never exceed it
  (`scroll_area.rs:763-765`), so a list that shrank once stays short. `SearchableComboBox`
  calls `Ui::set_min_height` with the row count's natural height before drawing the list. It
  must be `set_min_height` and never `set_max_height`: the latter unions `max_rect` with
  `min_rect` and then assigns `cursor.min.y = max_rect.min.y` (`placer.rs:248-258`), which
  drags the cursor back above the widgets already emitted — that is how the search field
  ended up painted over the first row. `set_min_height` goes through
  `Region::expand_to_include_y` (`placer.rs:274-281` + `layout.rs:67-71`) and only extends
  downward. Salting the popup's id instead would drop the widget's own state and force egui's
  INVISIBLE sizing pass (`area.rs:444` + `:623-624`) — a blink on every keystroke.
- `SearchableComboBox`'s per-item font resolver is called ONLY for the rows drawn in the
  current frame plus the selected row on the closed button. That is a contract, not an
  optimisation: egui's `add_font` never evicts, so a resolver called for every filtered row
  every frame would grow the font atlas without bound while the user scrolls a catalog.
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
- To change the preset grid, its palette defaults, or the update/cancel semantics, edit
  `color_preset_picker.rs`; to change WHERE a preset set comes from or goes, edit the caller,
  not the widget.
- To change jamo keyboard layout or latch semantics, edit `hangul_keyboard.rs`; the syllable
  arithmetic and the compatibility-jamo tables belong to `crates/ms-text-util/src/hangul.rs`.
- To change panel docking (arrangement rules, gaps, shrinking, or later the panel widgets), edit
  `panel_dock/` and read its own `MODULE_README.md` first.
