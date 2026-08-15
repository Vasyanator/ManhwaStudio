# Module: src/canvas

## Purpose
This directory implements the shared egui canvas used by translation, cleaning, and typing.
It owns page layout, viewport navigation, bubble editing, and the runtime layer for clean
overlay display and editing. Tabs customize behavior through hooks instead of owning separate
canvas interaction code.

## Architecture
`CanvasView` is the facade used by tabs. The canvas keeps tab-specific behavior behind
`CanvasHooks`, while shared page, bubble, overlay, and settings runtime state lives in
submodules. Expensive clean-overlay tiling and settings writes run through background
workers; GPU texture upload is throttled on the GUI thread. Clean-overlay GPU tile caches
report memory snapshots and can be evicted under memory pressure; CPU overlay images stay
owned by the model/runtime so edits and export payloads remain intact.

Bubble editing is split between runtime state and UI layers. `bubble_runtime.rs` owns pending
upserts/deletes, shared-model sync, undo/redo snapshots, clipboard flows, and failed-write
preservation. Bubbles have a domain class (`BubbleClass::Text` / `Image` / `Hint`, persisted as the
`bubble_class` wire token); text bubbles may render as aside/on-top/default, while image and hint
bubbles always use the aside layout path. `bubble_aside_ui.rs`
and `bubble_on_top_ui.rs` only handle layout, hit rectangles, focus, drag, and resize widgets.

A hint bubble (`BubbleClass::Hint`) is a one-line author note anchored to a page point, meant for
injection into the composed translation prompt. It reuses the ordinary aside text-bubble geometry
(anchor, link line, resizable rect) verbatim; its single line lives in `Bubble.text`. It owns one
`extra` key, `hint_show_outside_translation` (bool, absent == false), mirrored onto
`RuntimeBubble::hint_show_outside` by `upsert_runtime_from_bubble` and by `patch_bubble_extra_fields`
(the latter is required: patching advances the model revision, so the model→runtime sync would not
re-apply it and the next positional flush would write the stale runtime value back). Creation goes
`create_hint_bubble_at_pointer_shortcut` → `create_bubble_at` → `promote_bubble_to_hint`, which seeds
the flag from the user-level `CanvasState::hint_show_outside_default`.

CONVERTING an existing bubble goes through `mod.rs::set_bubble_class_for_bid`, which owns the whole
class-owned normalization (the bubbles side panel only calls it and mirrors the result into its own
editor buffer). Entering `Hint` pins `bubble_type = Aside`, seeds `hint_show_outside` from the same
user-level default, and folds the two text fields into the hint's single line: a blank `text` takes
over `original_text`, while a non-empty `text` deliberately LEAVES the original in place rather than
destroying user-typed text (no hint code path reads it, and converting back to `Text` restores it).
Leaving `Hint` clears the runtime flag, and `flush_bubble_upserts_to_model` REMOVES
`extra["hint_show_outside_translation"]` whenever the class is not `Hint` — the key is class-owned,
so a hint → text → hint round trip re-seeds from the current default instead of resurrecting a stale
per-bubble value.

Editable hint card anatomy (`bubble_aside_ui.rs`): no header, no ORIGINAL field, one SINGLE-LINE
`SpellcheckedTextEdit::singleline` bound to `Bubble.text` (id salt `("aside_hint", bid)`), an action
row with the delete button only, then the tab-owned footer. It registers in the same per-frame machinery as
the ordinary fields (`note_focused_bubble_text_input` under `BubbleTextField::Translation`, since the
hint line lives in the same buffer the translation field edits, plus `schedule_text_upsert` /
`commit_text_upsert_now`). `AsideBubbleVisibleGroups` therefore carries SEPARATE
`show_translate_action` / `show_delete_action` flags — a hint keeps delete and drops translate — and a
`show_hint_text` flag. `aside_body_mode` pins hints (like image bubbles) to `Full`, so the compact
modes never reshape them, and `estimate_aside_body_height` has a matching hint branch: a shape change
that is not mirrored there makes the packer reserve a wrong slot height for a frame. A read-only hint
(shown in cleaning/typing because its checkbox is on) uses the ordinary read-only label path.

An image bubble is a *group* of text areas (`RuntimeBubble.text_areas`, persisted via
`extra["text_areas"]`; see `parse_image_text_areas` / `serialize_image_text_areas` in `helpers.rs`).
The red `rect_coords` is the single image-area rectangle: drawn red, movable, and resizable via 8
handles; it is not a text area. For page-crop bubbles it is the crop region — `crop_rect` is kept
equal to `rect_coords` on save and `helpers::image_area_rect_from_bubble` resolves the crop region
as the image area, so the canvas owns the only red rect (the translation tab draws no separate crop
overlay). Every text area (including area 0) is an independent image-space sub-box clamped inside
`rect_coords`, with its own anchor (inside its rect), text, resize handles, and palette color
(`image_area_palette`, a reverse rainbow from blue). The bubble side comes from
`image_bubble_side_from_areas` (sign of `Σ(anchor_u − 0.5)`). Editable, one card holds the preview,
one framed row block per area (`Оригинал`/`Описание`/`Перевод`), an "add area" button, then the
action row; each area draws its own colored rect, anchor point, and link line (aimed at that block's
center). The aside column is built from `AsideItem`s, not raw bubble ids: a read-only image bubble
splits into one `AsideItem` per area, each rendered as an ordinary text-only aside card placed by
its own anchor side. Drag routing (`AsideDragTarget`): a row block moves only its area; the card body
outside blocks, or empty space inside the red rect, moves the red rect (areas + anchors follow); an
area rect on the page moves that area; an anchor point moves only inside its own area.

The canvas owns a collapsible bottom-center keyboard-shortcut hint overlay, drawn centrally in
`scene.rs::draw_canvas_bottom_hint` right after the floating controls. Content is opt-in per tab:
the owning tab pushes `Option<CanvasBottomHint>` (label + key rows) every frame via
`set_bottom_hint`, and `None` hides it. Rows are FIXED, hand-authored, localized pairs (built in
`app.rs::build_translation_hint_rows` / `build_typing_hint_rows` from `canvas.bottom_hint.*` `t!`
keys), NOT derived from the hotkey registry; rebuilding them per frame re-localizes on a runtime
language switch. A row may also carry an optional `CanvasHintHelp` (`CanvasHintRow::with_help`):
a circled "?" (`widgets::HelpHint`) whose hover tooltip shows text, an animation, or both, drawn
between the label and the keys. `scene.rs::draw_hint_rows_grid` switches to a three-column grid
(plus `min_col_width(0.0)`, since the default ~40pt minimum would inflate the 14pt icon column)
as soon as ANY row has help, and keeps the exact two-column layout otherwise; the same grid also
backs the shortcuts chip, whose rows must stay help-free because it renders inside a tooltip.
Only Translation and Typing set a hint; the Cleaning tab leaves `bottom_hint` at
`None`, so the overlay is not drawn there. The overlay is bottom-pinned just above the horizontal
scrollbar (or the inner viewport bottom when no bar is drawn) and never overlaps the bar. Collapsed
shows only an up-arrow toggle; expanded slides a popup panel up with the arrow riding above it.
`bottom_hint_collapsed` is the live user toggle: seeded once from `user_config` via
`set_bottom_hint_collapsed` at tab construction (default expanded) and read back with
`bottom_hint_collapsed()` for persistence — persisted only for Translation and Typing (Cleaning has
no hint). Per-tab content and config persistence live in the tabs, not here.
`scene.canvas_bottom_hint_rect` (last-frame rect) occludes canvas zoom/drag input under the hint.

The canvas also owns the floating Hangul jamo keyboard (`widgets::show_hangul_keyboard`), opened
from the ORIGINAL/TRANSLATION context menu of a text bubble. `BubbleMenuContext::field` carries
which field was right-clicked (`None` for the bubble-body menu) and gates that item; the item
returns the deferred `BubbleMenuCommand::OpenHangulKeyboard(BubbleTextField)`, applied in
`apply_bubble_menu_command` like every other state-mutating menu action.

The panel is DECOUPLED from its insertion field. Two independent pieces of state model this:
`bubble_runtime.hangul_keyboard: Option<HangulKeyboardState>` is "the panel is open" and holds ONLY
the widget's latch/mode/placement state; `bubble_runtime.hangul_target: Option<HangulInsertTarget>`
(bubble id, field, captured `TextEdit` id) is a STICKY pointer to the last bubble text field that
held keyboard focus. The target is NOT rebuilt every frame like `focused_text_input`: clicking a
panel button steals focus off the `TextEdit`, so a per-frame target would vanish the moment the
user acts. It persists until another insert-eligible field is focused, its bubble is removed, or the
project changes. `open_hangul_keyboard_session` no longer binds: it opens the panel, seeds the
target from the right-clicked field (resolving the `TextEdit` id from the per-frame
`bubble_text_edit_ids` registry, falling back to `focused_text_input`; opens with `None` target and
logs a warning if the field was not drawn this frame), and pre-latches the widget from the syllable
before the field's caret (`hangul_seed_at_caret`, char only — the range is discarded).

Replace-previous is now computed LIVE, so there is no stored range to go stale under an OCR/MT
rewrite (the old seed machinery is gone). `apply_hangul_keyboard_insert(target, insert,
replace_previous, …)` reads the CURRENT caret each time (from `focused_text_input` when it still
matches the target, else egui's stored cursor for `target.text_edit_id`, else end-of-text; clamped)
and turns it into the splice range via `hangul_insert_splice_range`: a non-empty selection is
replaced in both modes, a collapsed caret at `p` with `replace_previous && p > 0` targets the single
char before it (`(p-1)..p`), otherwise a plain insert at `p`. One `char`, not a grapheme cluster —
precomposed syllables and compat jamo are single scalars. The splice goes through
`helpers::splice_char_range` (always char↔byte conversion), then caret + focus are restored through
`target.text_edit_id`. The runtime write uses the same `schedule_text_upsert` +
`commit_text_upsert_now` pair as `apply_paste_text`, and additionally stages
`capture_bubble_history_before_mutation` first — which `apply_paste_text` does NOT do — so one
insert becomes one bubble-history entry. That entry is not what Ctrl+Z hits right after an insert:
the insert path deliberately restores focus to the `TextEdit` (the right UX), and `handle_shortcuts`
gates the bubble history on `!ctx.egui_wants_keyboard_input()`, which in egui 0.35 is
`memory.focused().is_some()`. So while the field is focused Ctrl+Z is consumed by egui's own
`TextEdit` undoer; the bubble-history entry is what applies once focus is elsewhere.

`mod.rs::draw_hangul_keyboard_panel` draws the panel after the scene pass as an `egui::Window` with
a literal id, owns closing through `Window::open`, and publishes its rect into
`scene.canvas_hangul_keyboard_rect`, which is folded into the same `handle_shortcuts` `inside_canvas`
occlusion test as `canvas_bottom_hint_rect` — without it the wheel over the panel would zoom the page
underneath. The rect is published only on the keep-open path: `handle_shortcuts` consumes last
frame's value, so publishing it on the frame the window is closed would swallow one further frame of
Ctrl+wheel zoom over dead space. Each frame it resolves the CURRENT valid target: "valid" now means
DRAWN this frame, i.e. present in `bubble_text_edit_ids` — existence alone is not enough, because an
on-top ORIGINAL field after deselect or any bubble scrolled off-screen still has runtime text but no
live widget, and an insert there would splice off-screen at a stale caret whose focus restore no-ops.
A TRULY-gone target (its bubble/field no longer exists at all) is still evicted, but a target that was
merely not drawn this frame stays sticky and becomes valid again when the field scrolls back into view.
The registry also owns the field's current id, so the panel takes `text_edit_id` from it as the single
source of truth. When there is no valid target it draws a RED warning line
(`canvas.hangul_keyboard.no_target_warning`) above the keyboard and DROPS any insert — the panel stays
open (losing a target is not a reason to close; that is the whole point of unbinding). On a successful
Compose insert the panel clears the widget latches (via `HangulKeyboardState::clear`) — the widget no
longer self-clears on Insert, so a dropped insert keeps the composition, and a Direct-mode jamo insert
never clears the surviving latches. The panel has exactly two close paths — the user closing the window, and a project
switch — and TWO target-eviction points: `remove_runtime_bubble` clears `hangul_target` if it points
at the removed bubble (the panel stays open and shows the warning), and
`close_hangul_session_on_project_change` (called at the top of `sync_runtime_from_model_or_project`)
clears BOTH the panel and the target. The second is required because bubble ids are per-project: a
resync into another project updates colliding ids in place through `upsert_runtime_from_bubble` and
never reaches `remove_runtime_bubble`, so an open panel would silently retarget. Its signal is
`project.paths.bubbles_file`, which changes exactly on a project switch and never on an edit.

`note_focused_bubble_text_input` records THREE things per drawn field: the widget `Id` into the
per-frame `bubble_text_edit_ids` registry (unconditionally — a right-click does not focus an egui
`TextEdit`), and, only for the focused field, the caret into `focused_text_input` AND the sticky
`hangul_target`. The registry and `focused_text_input` are cleared at the top of each `draw`;
`hangul_target` is not (that is what makes it sticky). The widget id is always CAPTURED from
`response.id`, never reconstructed from a salt: of the four registered field flavours, three
(`aside_original`, `aside_translation`, `on_top_original`) derive an absolute `Id::new(salt)` through
`SpellcheckedTextEdit::id_salt`, while `on_top_text` uses a ui-scoped `make_persistent_id` that
cannot be reproduced outside its owning `Ui`. One captured id covers both derivations and cannot
silently drift when a salt is renamed.

The canvas' own controls («Лента») are a DOCK TAB, not a panel of the canvas' own making. The
canvas owns the tab's identity and body — `CANVAS_RIBBON_TAB` (the literal `"canvas.ribbon"`),
`CanvasView::ribbon_tab_title` (the one lookup of `canvas.ribbon.tab_title`),
`CANVAS_RIBBON_TAB_MIN_SIZE_PX` / `CANVAS_RIBBON_TAB_INITIAL_SIZE_PX`, and
`scene.rs::draw_ribbon_tab_body` (page counter, zoom row, "show bubbles", bubble opacity) — while
the panel it lives in, its position, size and collapsed state belong to the panel dock and are
persisted per program tab. All three canvas tabs declare that one tab from
`CanvasHooks::draw_canvas_overlay_top_left`, i.e. still inside `CanvasView::draw` and therefore
BEFORE `publish_canvas_settings`, so an edit lands in the same frame it is made. `dock_area_rect`
is the shared rule for the area they hand the dock: `canvas_rect` minus
`CANVAS_DOCK_AREA_SCROLLBAR_RESERVE_PX` on the right, so no panel can sit on the vertical
scrollbar.

Its zoom row also carries the canvas-shortcuts hover chip (`canvas.shortcuts_hint.*`),
right-aligned via `egui::containers::Sides` in `draw_zoom_and_shortcuts_row`; the chip lists the
canvas-intrinsic navigation keys (zoom/pan/scroll) and is hover-only, so it has no click and no
persisted state. `Sides` stretches the row to the full available width, which is exactly what the
pre-dock `Area` could not afford (auto-sized, "available" was the whole screen, and the stretched
row fed its own width back in and grew the panel by one item-spacing per frame — hence the old
`controls_content_width` measurement). A dock body has a finite width, and the dock re-measures a
tab's HEIGHT only: `CollapsiblePanel` reports back the width it was GIVEN, never the drawn one, so
stretching cannot become a size request. When adding a row here, keep that asymmetry in mind — a
row's height is a request, its width is not, which is why the initial width is derived from the
widest row by hand (see the constant's own comment).

Clean overlays enter through `CleanOverlaysModel`. Normal canvas visibility uses the
model's shared visibility flag. A canvas may also set a local clean-overlay visibility
override for UI-only cases such as the typing tab; local overrides must not mutate the
shared model or change cleaning-tab visibility.

Viewport sync across translation, cleaning, and typing is explicit. `MangaApp` owns the
shared `CanvasViewportSnapshot`, publishes it only from the active canvas after that
canvas is drawn, and applies it only to the canvas being entered. Inactive canvases must
not be scrolled or re-anchored every frame.

`CanvasView::focus_page` is deferred navigation: it applies the zoom immediately and records
a `pending_focus` request that the draw pass resolves into a scroll offset once the target
page's world rect (and its center, when not passed explicitly) is known — so focusing a canvas
that has never been laid out (e.g. opening a page in a not-yet-visited tab from the page
manager) scrolls on its first frames instead of being silently dropped. Exactly one request is
pending at a time; a newer `focus_page` or `apply_viewport_snapshot` call replaces or drops it.

Source page geometry is separate from source page GPU residency. Scene layout and hit testing use
`PageImageInfo` dimensions supplied by `MangaApp`; `PageTexture` only represents optional tiled GPU
handles for source imagery. NEAREST source textures are materialized lazily while pixel inspection
is active and are dropped outside the active page window.

Pixel inspection has a single DPI-correct trigger: `device_pixels_per_source` (`zoom *
pixels_per_point`) compared against `PIXEL_INSPECTION_MIN_DEVICE_PX`, exposed as
`pixel_inspection_recommended`. The same notion drives NEAREST sampling for source tiles, the clean
overlay, and the cleaning text mask, plus the pixel grid, so a magnified source pixel looks identical
across layers. The grid is drawn in one late overlay pass (`draw_pixel_grid_overlay`), not in base
layers. Overlay and text-mask tile draws viewport-cull tiles against the visible clip rect.

Directed zoom is anchored in content/world space and clamps the requested horizontal
scroll offset to the current scrollable range. The canvas creates horizontal scroll range
before the visual strip fully reaches viewport width, so anchor compensation has a stable
X range before the old overflow point.

## Files and submodules
- `mod.rs`: public facade, hook trait, render orchestration, and synchronization with
  shared models.
- `scene.rs`: page strip layout, viewport interaction, page hit-testing, and the canvas' own
  viewport UI (`draw_ribbon_tab_body` — the «Лента» dock tab's body — and the bottom hint).
- `overlay_runtime.rs`: clean overlay CPU/GPU runtime state, background preparation, and
  local/shared visibility state.
- `bubble_runtime.rs`: runtime bubble state, model synchronization, undo/redo, and clipboard.
- `bubble_aside_ui.rs`: aside bubble column layout and interactions. Layout runs as
  `build_aside_desired_slots` (measure) -> `pack_aside_slots` (pure vertical packing) ->
  `draw_aside_slots`. `draw_aside_side` picks single- or two-column layout per side: with
  `CanvasState::aside_second_column` on and enough free span for both columns plus gaps to stay
  inside the viewport, a side splits into near/far columns. Distribution is near-priority
  (`split_near_priority`): isolated bubbles stay near, only overlapping clusters split alternately
  near/far. Columns are equal width (min width, hugging the ribbon, when stretching is off; up to
  max width when on). Far links stay roughly horizontal while the near column packs invisible spacers
  at far anchor heights so its cards spread and far links thread the gaps.
- `bubble_on_top_ui.rs`: on-page bubble widgets, focus controls, move, and resize handling.
- `settings.rs`: canvas settings snapshots and persistence worker. Its `user_config.json` half writes
  through `config::update_user_config_file` (the serialized read-modify-write boundary), never a bare
  `fs::write`: this worker runs off the GUI thread and an unlocked read-modify-write here could drop
  the ORT SIGILL guard marker written concurrently under the same lock.
- `helpers.rs`: stateless geometry, image, and text helper functions.
- `types.rs`: passive DTOs and runtime payload types.
- `view_transform.rs`: `ViewTransform` world<->screen affine map (`screen = world * scale + translation`). The `ScrollArea` still allocates the page strip and owns scrolling, but each page's authoritative screen `image_rect` and its `page_in_view` visibility are now produced by this transform: `reserve_canvas_page_frame` establishes one per-frame transform from the first laid-out page (`scale == state.zoom`, `translation = old_image_left_top - world_min*scale`) and maps every page through `world_rect_to_screen`. A once-guarded equivalence check warns if the transform-derived rect drifts >0.5px from the old ad-hoc rect. Future increments will remove the `ScrollArea` and make the transform the sole camera.
- `workers.rs`: background worker startup for overlay preparation, autosave, and settings.

## Contracts and invariants
- Do not block the GUI thread with image decoding, disk I/O, long computation, or worker waits.
- Do not hold shared model locks while rendering, calling hooks, or doing heavy work.
- Keep page pixels, scene coordinates, screen coordinates, and UV coordinates explicit.
- Overlay buffers and masks must validate width, height, and buffer length before use.
- Shared visibility changes belong in `CleanOverlaysModel`; tab-local visibility must stay
  inside the specific `CanvasView`.
- Canvas scroll areas need per-instance egui ids. Cross-tab viewport sync must go through
  `CanvasViewportSnapshot`, not shared egui `ScrollArea` memory.
- `CANVAS_RIBBON_TAB` (`mod.rs`) is the ONE definition of the controls tab's identity, and the
  canvas does NOT own the panel it is drawn in. The canvas also owns the tab's DECLARATION
  (`declare_ribbon_tab`) and the default one-panel arrangement of a program tab that declares
  «Лента» and nothing else (`ribbon_only_dock_layout`, «Перевод»), so the three call sites cannot
  drift. A tab that wants to
  place other floating UI under that panel asks the DOCK for the rect
  (`PanelDockOutput::tab_rect(CANVAS_RIBBON_TAB)`), which answers for the MAIN window alone: it is
  `None` while the tab is hidden AND while the user keeps it in a detached sub-window, whose rect
  lives in that window's own coordinate frame. Both are "not on screen" and must not be
  approximated. The dock output only exists after the dock has run, so a surface drawn earlier in
  the same hook necessarily uses the PREVIOUS frame's rect (translation's two text-detector edit
  boxes do exactly that).
- The canvas never touches `PanelDockState`. It only CARRIES the app-owned borrow from
  `CanvasDrawParams::panel_dock` into `CanvasHooks::draw_canvas_overlay_top_left`, which is where
  every canvas program tab runs `PanelDock::begin … end`. Running the dock there — rather than
  after `CanvasView::draw` returns — is what keeps a «Лента» edit ahead of
  `publish_canvas_settings`. It decides NOTHING about z-order: within one `Order` egui keeps a
  persistent layer list and re-sorts it stably each pass, so creation order inside the frame is
  irrelevant; a layer rises only when `Area::begin` finds it was not visible last frame
  (`egui-docs/06-overlays.md` §1.1). The standing consequence is that a tab's full-canvas
  `Order::Foreground` capture surface floats above the dock's panels for as long as its selection
  mode lasts and takes the clicks meant for them — the same clicks the pre-dock controls panel lost
  on the lower `Order::Middle`, i.e. known behaviour, not a regression to "fix" by reordering.
- The canvas does NOT load fonts. Bubble text is drawn with the egui family named by
  `helpers::BUBBLE_TEXT_FONT_FAMILY_NAME`, which is an alias of
  `crate::ui_fonts::BUBBLE_TEXT_FAMILY_NAME`; the chain behind that family is installed once
  per window by `crate::ui_fonts` from `fonts/ui`. `helpers::bubble_text_font_id` still
  degrades to the default family when nothing is bound yet, so the canvas stays correct
  during the frames before the background font loader finishes.
- It does, however, TRIGGER the extended tier. The studio arms `fonts/ui/ext` instead of
  loading it, so `bubble_aside_ui.rs` / `bubble_on_top_ui.rs` offer every bubble string they
  are about to draw to `crate::ui_fonts::ensure_covers` (once per card, where the strings
  are already in hand); the first character the chain cannot draw starts the background
  install. The call is idempotent and returns on a single atomic load once the question is
  settled, so it stays on the per-frame path. Keep these call sites to the places that
  actually draw chapter text — the point is a real coverage question, not a broadcast.
- The page strip in `scene.rs` deliberately zeroes the ambient `item_spacing` while
  allocating page rows so screen tops stay linear in world space and the `ViewTransform`
  (`screen = world*scale + translation`) can reproduce them. Inter-page gaps come only from
  the explicit `edge_margin`/`page_spacing` settings, never from theme spacing. The spacing
  is restored before drawing aside/on-top bubbles, whose inner widgets inherit the ui style.
- `CanvasHooks` callbacks must stay lightweight and must not mutate shared models while canvas
  locks are held. Use typed canvas APIs or tab-owned worker/event channels for heavier work.
- Vertical-scrollbar marks are tab-owned. After `draw_canvas_scene` lays out the strip,
  `mod.rs::render_scrollbar_marks` asks the active tab via `CanvasHooks::canvas_scrollbar_marks`
  (default none) and paints the returned marks onto the native vertical bar with
  `widgets::paint_marks_on_bar`, then re-draws the handle on top so it stays visible. The
  `egui::ScrollArea::both` engine is untouched (both axes scroll natively). Tabs position marks in
  content space via `CanvasScrollbarContext::content_y` (`world_y * zoom` from
  `scene.page_world_rects`); the canvas owns geometry, the tab owns mark content.
- Bubble persistence is routed through `BubblesModel` saver tasks; canvas runtime should keep
  unsaved runtime edits explicit until they are flushed to the model.
- The periodic overlay autosave has an explicit shutdown flag and is joined before structural page
  operations or app teardown; no autosave writer may survive a page-index transaction.
- Bubble undo/redo is delegated to the generic `ms-actions` engine
  (`bubble_runtime.rs::bubble_history: ActionHistory<BubbleSnapshotOp>`; the op lives in
  `bubble_action.rs`). It is a behavior-preserving FULL snapshot op, not a field-level patch:
  each op holds `Arc<Vec<Bubble>>` before/after snapshots and reverses by `BubblesModel::reset`.
  Mutation is observer-style — the call site mutates the model directly, then history is recorded.
  `capture_bubble_history_before_mutation` stages the pre-mutation snapshot + revision in
  `pending_history_before`; the next capture (or an undo/redo) finalizes it into a recorded op via
  `finalize_pending_history`, using the then-current state as the mutation's `after`. Recording is
  deduplicated by revision (monotonic, bumped per mutation): a staged snapshot whose revision still
  matches the current model produced no mutation and records nothing, so one gesture is one op. The
  engine enforces the `BUBBLE_HISTORY_LIMIT` cap and truncates the redo branch on a fresh record.
  `flush_bubble_upserts_to_model`
  debounces positional model writes while a continuous drag/resize gesture is active
  (`aside_drag_state` / `on_top_drag_state` / `active_rect_handle` / `active_area_handle`): the
  runtime bubble still follows the pointer each frame, but the model is written only on release,
  so one gesture yields exactly one undo entry and one model commit. Gesture-end handlers must
  re-insert the dragged id into `pending_upsert` so the final position commits. If the dragged
  widget stops being rendered mid-drag (its page scrolls fully off-screen) egui never delivers
  `drag_stopped()`, so the per-frame `mod.rs::commit_lingering_drag_gestures_on_pointer_up`
  fallback (run in `draw` after the scene pass, only when the primary pointer is up) is the
  data-loss guard: it routes aside/on-top drags through `finish_*_drag` and mirrors the rect/area
  handle `drag_stopped` paths (`pending_upsert.insert` + clear `active_*_handle`). It is the single
  source of truth for that commit and is skipped for a normally-finishing gesture, which already
  cleared its state before the fallback runs, so each gesture commits exactly once.
- `hook_bubbles_revision()` is a cheap `u64` fingerprint of the bubble set `hook_bubbles_snapshot`
  would build: it folds `BubblesModel::revision()` with the runtime-only set
  (`runtime_bubbles` count + `next_bubble_id`), so a runtime-only, not-yet-flushed bubble bumps it.
  Use it for equality gating between frames, not for ordering.
- `page_bubbles_bucketed(page)` buckets all runtime bubbles of a page into the four
  `(side, type)` aside/on-top columns in a single pass. It is the sole bubble-column scanner;
  consumers read the relevant `(side, type)` column from one bucketed result per page per pass
  instead of re-scanning runtime bubbles once per column.
- HIDDEN HINTS ARE GATED IN ONE PREDICATE: `bubble_runtime.rs::is_runtime_bubble_hidden(&rt)` is
  `class == Hint && !editable && !hint_show_outside`. `editable` is the tab discriminator
  (translation keeps it `true`; cleaning and typing set it `false`). "Hidden" means the bubble does
  not exist for this canvas: not bucketed, not drawn, not hit-testable, not focusable, and NOT
  COUNTED FOR LAYOUT. Any new scan over `runtime_bubbles` that feeds drawing, hit-testing, or
  layout must call the predicate — a scan that forgets it leaks the bubble's existence even without
  painting it. There are exactly three call sites today: `page_bubbles_bucketed` (covers both aside
  columns, both on-top columns, and `focus_candidate_at_scene_pos`), `mod.rs::refresh_page_aside_presence`
  (the aside-gutter reservation read by `scene.rs::canvas_row_width_for_page`; without it a page whose
  only aside bubble is a hidden hint would reserve an empty gutter), and
  `bubble_aside_ui.rs::aside_hit_test` (whose `mounted` flag stays `true` with stale card geometry
  when a shown hint is switched off, so the gate — not `mounted` — is what stops a click from being
  swallowed by an invisible card).
- Neither `BubbleClass::Image` nor `BubbleClass::Hint` is a display type. Both must not resolve
  through on-top display settings: `displayed_bubble_type_for_runtime` forces `BubbleType::Aside`
  and `set_bubble_class_for_bid` pins `bubble_type = Aside` when converting into either class.
  Class-specific metadata belongs in bubble `extra`.
- `bubble_fingerprint_with_hasher` must cover the domain class and every class-specific `extra` key
  the canvas renders or edits (`bubble_class`, `text_areas`, `description`,
  `hint_show_outside_translation`). A change invisible to the fingerprint does not propagate to
  other tabs.
- Per-bubble image caches on `CanvasView` (`image_bubble_meta_cache`,
  `image_bubble_preview_cache`, keyed by bubble id) must be evicted whenever a bubble is fully
  removed. `remove_runtime_bubble` is the single full-removal path and owns that eviction, so
  deleted ids never leak and a reused id cannot serve a stale fingerprint/preview.
- Source page GPU residency is verified manually for now because `egui::TextureHandle` creation
  and eviction require a live GUI context; pure tests should target memory-manager policy instead.
- Clean-overlay memory eviction may drop only reconstructable GPU texture pages. It must not drop
  `overlay_images`, prepared worker payloads currently being uploaded, or shared model state.

## Editing map
- To change clean overlay visibility, upload, tiling, or editing runtime, edit
  `overlay_runtime.rs` and the facade methods in `mod.rs`.
- To change page layout, scrolling, zooming, or context menus, edit `scene.rs`.
- To change what the «Лента» tab SHOWS, edit `scene.rs::draw_ribbon_tab_body`; to change how big it
  starts or how small it may get, edit the two `CANVAS_RIBBON_TAB_*_SIZE_PX` constants in `mod.rs`.
  Where the panel holding it sits by default is a per-program-tab decision: «Текст» and «Клининг»
  have their own builders (`typing_default_dock_layout`, `cleaning_default_dock_layout`), because a
  default layout must name every tab its program tab declares; «Перевод» declares «Лента» alone and
  uses `mod.rs::ribbon_only_dock_layout`. To change how the tab is DECLARED (title, size
  bounds, body), edit `mod.rs::declare_ribbon_tab`, which all three tabs call.
- To change source page GPU residency or NEAREST inspection behavior, edit `scene.rs`,
  `mod.rs`, and the source-page texture owner in `app.rs`.
- To change bubble editing behavior, start in `bubble_runtime.rs` and the relevant
  bubble UI module.
- To change hint-bubble behavior, edit `bubble_runtime.rs` (`create_hint_bubble_at_pointer_shortcut`,
  `promote_bubble_to_hint`, `is_runtime_bubble_hidden` and its three call sites, the write-back in
  `flush_bubble_upserts_to_model`), `helpers.rs::hint_show_outside_from_extra`, and
  `mod.rs::set_bubble_class_for_bid` (forced aside + class-owned normalization). For the CARD, edit `bubble_aside_ui.rs` (`aside_visible_groups`,
  `estimate_aside_body_height`, and the `show_hint_text` field in the card body) — the estimator and
  the card body must change together. The footer content is tab-owned
  (`tabs/translation/tab.rs::build_bubble_footer`). The default for new hints is a canvas setting
  (`CanvasState::hint_show_outside_default`, edited in `src/tabs/settings/canvas_ribbon.rs`).
- To change the Hangul keyboard wiring (panel open/close, the sticky insert target, live caret
  capture, the text splice), edit `bubble_runtime.rs` (`open_hangul_keyboard_session`,
  `apply_hangul_keyboard_insert`, `hangul_insert_splice_range`, the `hangul_target` eviction points);
  for the window itself, its default position, the no-target warning, or its input occlusion, edit
  `mod.rs::draw_hangul_keyboard_panel` and `scene.canvas_hangul_keyboard_rect`. The keyboard
  content is a general-purpose widget and lives in `src/widgets/hangul_keyboard.rs`.
- To change canvas hook contracts, public runtime DTOs, or persisted canvas settings, start in
  `types.rs`, `mod.rs`, and `settings.rs`.
- To change background preparation or settings-save threading, edit `workers.rs` and the caller
  runtime module that owns the channel.
