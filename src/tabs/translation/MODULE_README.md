# Module: src/tabs/translation

## Purpose
Translation tab runtime for OCR, text detection, machine translation, translation-focused bubble
editing, composed text export, and translation-specific canvas hooks.

## Architecture
`TranslationTabState` in `tab.rs` is the tab orchestrator and `CanvasHooks` implementation. It owns
panel state, controller instances, footer metadata caches, text-detection storage, OCR selection
state, and debounced settings persistence.

Long work is delegated to focused controllers:

- `ocr.rs` owns the OCR worker, backend IPC transport (framed `shared_client()`), AI API OCR
  transport, page crop/cache handling, OS credential-store API key access, and per-engine load state.
- `text_detector.rs` owns the text detector worker and returns page boxes plus editable binary
  masks.
- `machine_translation.rs` owns MT run threads, AI API MT chat batching/context pruning,
  cancellation, and stale-event filtering.
- `backend_health.rs` owns the shared Python backend probe snapshot used by Translation and
  Settings.

UI panels under `panels/` expose typed options/actions and avoid owning worker lifecycles. The tab
converts panel actions into controller requests, canvas changes, persistent settings, or storage
jobs.

This tab also HOSTS the shared panel dock (`src/widgets/panel_dock`). It declares two tabs — the
canvas' own «Лента» (`canvas::CANVAS_RIBBON_TAB`, body `CanvasView::draw_ribbon_tab_body`, declared
through `canvas::declare_ribbon_tab`, the canvas' one declaration of it) and its own «Последние
персонажи» (`TRANSLATION_RECENT_CHARACTERS_TAB`, body `draw_recent_characters_tab_body`) — through
the per-frame context `TranslationDockCx`. Its default arrangement is its OWN
`translation_default_dock_layout`: the ribbon panel at the dock area's left edge and the cards
docked to its right, top-aligned, which is where the row of cards floated before the migration. The
same `fn` item is handed to the dock both by `app.rs::restore_panel_dock` and by
`ensure_default_layout`, since the dock state keeps it to rebuild the arrangement for the header's
reset item.

«Последние персонажи» is declared on EVERY frame with `visible(!recent_characters.is_empty())` and
`transparent_until_hover(true)`. Visibility, not declaration, is what an empty history switches: a
hidden tab keeps its slot in the layout, so the cards return to wherever the user put them, while a
skipped declaration would make the dock treat the tab as another program tab's and seed it a fresh
panel. Transparency is what keeps the cards looking exactly as they did as a bare `egui::Area` —
the panel paints no frame, border, arrow or grip until the pointer is over it, and geometry is
identical in both states. The cards are display-only in both states too: a character is activated by
its digit together with the bubble-create hotkey or the Shift+drag OCR gesture, never by a click.

A card is PAINTED, not built from an `egui::Frame`, and that is a contract of the body rather than a
style choice. `Frame` reserves its space through `Ui::allocate_rect`, which advances the cursor past
an explicit rect and never reaches `Placer::next_space` — the only place `Layout::next_frame` starts
a new row — so a framed card cannot wrap and the whole history runs off the right edge of the panel
into a horizontal scrollbar. `draw_recent_characters_tab_body` therefore lays the caption out first
(unwrapped, so a name never breaks across two lines), derives the card size from that galley
(`recent_character_card_size`), allocates that exact size through `Ui::allocate_exact_size`, and
paints the background, the border and the text into the rect the layout returns
(`recent_character_card_caption_pos`). The two size helpers are each other's inverse and are tested
as such: they are what keeps the hand-painted card pixel-identical to the framed one it replaced.

The dock state is app-owned (`MangaApp::panel_dock`, one per studio
window) and reaches this tab through `CanvasDrawParams::panel_dock`, which the canvas passes into
`draw_canvas_overlay_top_left`; the tab owns none of it. `draw_panel_dock` runs at the very END of
that hook, and what that placement buys is ONE thing: running inside `CanvasView::draw` keeps a
«Лента» edit ahead of `publish_canvas_settings`. It buys no z-order — within one `Order` egui's
layer list is persistent and re-sorted stably each pass, so creation order inside the frame decides
nothing (`egui-docs/06-overlays.md` §1.1). The standing consequence is that the OCR and image-crop
capture surfaces, full-canvas `Order::Foreground` areas, rise above the dock's panels every time
their selection mode is ENTERED (`Area::begin` raises an area that was not visible last frame) and
take the clicks meant for a panel until the mode ends — as they did from the pre-dock controls
panel on the lower `Order::Middle`. The panel's rect is stored in `ribbon_panel_rect` for the NEXT
frame, because the two text-detector edit boxes that hang under it are drawn before the dock runs;
it is `None` while the tab is hidden or detached, since `PanelDockOutput::tab_rect` answers for the
main window alone.

Text-detector mask GPU textures are a reconstructable display cache. The tab exposes memory
snapshots and coarse eviction methods for those textures, while detector result masks and stored
text-mask data remain intact for redraw, editing, and disk persistence.

Python-backed OCR/detector routes call the backend service over the framed IPC protocol on the
AF_UNIX socket from `backend_ipc::backend_socket_path()`. All backend transport goes through
`crate::backend_ipc::shared_client()`; `backend_health.rs` owns no TCP/port state. OCR requests use
`begin_call`/`CallHandle` for explicit cancellation (replacing legacy "latest-wins" behavior). App-managed
model files must be resolved through `src/ai_models.rs` before backend initialization; EasyOCR,
Surya, and PaddleOCR-VL library/Hugging Face caches remain backend/library-managed. PaddleOCR
detector-only downloads only detection files, while full PaddleOCR also downloads the selected
recognition language. PaddleOCR-VL (IPC method `ocr.paddle_vl`) is a PyTorch/Transformers OCR
engine that needs no text detection and no language selection; it is shown on a second engine row in
the OCR panel so the side panel stays narrow.
AI API OCR bypasses the Python backend and uses Rust `genai` from the OCR worker thread; provider
API keys must be read/written through the OS credential store and never persisted to project or
user JSON settings.
The native ONNX Runtime OCR path (MangaOCR + PaddleOCR) is selected in `ocr.rs` by the pure
`ocr_route` helper: with `General.ai_runtime == "native"` and a non-`Suspect` provider-scope SIGILL
guard, MangaOCR with an ONNX export (`base_onnx`/`2025_onnx`; `base_torch` has no native path)
routes to `NativeManga`, and PaddleOCR routes to `NativePaddle` (any language); every other case
(other engines, torch-only model, Suspect guard, backend runtime) routes to the Python backend. On
the native route the OCR worker decodes the crop to RGBA and calls
`crate::native_runtime::recognize_manga` / `recognize_paddle` (desktop-only), assembling lines/text
with the same `join_newlines`/`reflect_strings` rules as the backend (Paddle's lines are joined with
`\n` first). The guard scope uses `native_runtime::native_load_scope_key()` (provider[:device]) so the
pre-check matches the provider/adapter that will actually load. Native PaddleOCR text detection is selected in `text_detector.rs`
by `detector_native_route` (native runtime + non-Suspect guard): `detect_page_paddle_ocr` and the
inline `detect_paddle_mask_for_image` call `native_runtime::detect_paddle` and build the same
`TextDetectorPageResult`/mask the backend produces (xyxy blocks sorted y1,x1 truncated to 2500;
glyph mask normalized to 0/255, oversized masks over `MAX_MASK_PIXELS` rejected → backend fallback).
A native error (OCR or detection) is always logged. On the OCR recognize dispatch it falls back to
the backend ONLY when the backend is up; when the backend is offline the native path is the only
path, so `run_recognize_command` surfaces the real native error (`Нативный ONNX: <reason>` from
`NativeRuntimeError`) instead of the misleading "backend offline" string
(`try_native_ocr` → `NativeOcrOutcome`, `native_failure_should_surface`). These paths are compiled
out on the web build.
Native OCR/detection run WITHOUT the Python backend: BOTH the controller readiness gate AND the UI
trigger gate are route-aware. `ocr.rs::warmup_ocr_engine` and `text_detector.rs::run_detect_batch`
skip `ensure_v2_backend_ready` (and the backend warmup) when the active route is native — the
in-process runtime/model load lazily on first use and the controller reaches `Ready` with the
backend offline. The UI trigger gate `ocr::ocr_requires_backend(engine, model, runtime, guard)` is
the single source of truth for "does this OCR selection need the backend?": native ONNX routes
return `false`, backend routes and `AiApi` (over `genai`) preserve their historical behavior;
`tab.rs::ocr_requires_backend_runtime` calls it, reading the runtime+guard from a per-tab cache
refreshed at most every `OCR_ROUTE_INPUTS_CACHE_TTL` so the per-frame gate never blocks on disk I/O.
Every OCR trigger (drag-box, advanced/quick recognize, the load button, the proactive health check)
consults it, so a native selection dispatches with the backend offline. Only backend routes warm
the backend and probe backend health. The native→backend fallback still applies when native
inference fails AND the backend is up (the fallback path re-checks readiness itself); native no
longer requires the backend to run.
AI API MT also bypasses the Python backend. Plain untranslated text batches are sent grouped by
character and require flat JSON responses containing every bubble ID. When the "existing translation
in context" option is on, a scope translation also includes already-translated replicas as ordered
read-only context (`MtTranslateItem.needs_translation == false`, flagged `"context": true` with
their existing translation); such batches switch to the ordered per-item representation so the model
sees the exact reading-order interleaving. `split_ai_mt_batches` keeps context around the
translatable replicas but cuts each batch right after its `batch_size`-th translatable replica, so a
context replica beyond a reached per-batch limit is deferred until translation reaches its window;
context replicas are never returned, counted, or reported as failures. When ImageBubble inclusion is enabled for a
multimodal model, the MT worker attaches each image in ordered message parts using the selected
visual detail level and requires `original_text` plus `translation` for those IDs. A multi-area
ImageBubble is sent as one item whose `MtImageInput.areas` lists every text area (description,
current original, image-relative bbox); the model must return
`{id, areas:[{original_text, translation}, ...]}` and the per-area result is applied through
`CanvasView::apply_machine_translation_areas`. Binary image
parts are sent only in the active API request; retained chat history stores a text-only batch
record so image payload bytes do not consume the normal context budget. It keeps chat history
between batches and prunes old user/assistant turns when the configured context-fill budget is
exceeded. Progress events expose translated/error counts, approximate context usage, and the number
of replicas removed during context pruning; batches may include existing bubble translations when
the user enables that context option.
`BubbleClass::Hint` never enters an MT batch — not as a translatable item and not as context — in
any of the three collection paths (scope run, per-ImageBubble mode, explicit per-id request): a hint
is an author note addressed to the translator, not a replica.

## Files and submodules
- `mod.rs`: module wiring and public re-exports for `TranslationTabState`, hotkey IDs, and hotkey
  hint structs.
- `tab.rs`: main tab state, canvas hooks, panel routing, OCR region selection, advanced-recognition
  integration, footer metadata sync, text-detection mask/line editing, detector result storage,
  MT dispatch, and coalesced translation settings persistence.
- `ocr.rs`: `TranslationOcrController`, OCR load/recognize worker, framed IPC calls via
  `shared_client()` (with `begin_call`/`CallHandle` for cancel), AI API OCR via `genai`,
  credential-store API key commands, crop encoding, page image LRU cache, and
  model-download/load-state events.
- `ocr_case_fix.rs`: pure, GUI-free post-OCR ALL-CAPS normalization (detector +
  character state machine) used by `ocr.rs`'s post-processing helper.
- `text_detector.rs`: `TranslationTextDetectorController`, local classic detector, backend
  PaddleOCR/CTD/Surya detector calls, mask decoding/building, detector batch progress, and helper
  routes reused by cleaning tools for region masks.
- `machine_translation.rs`: `TranslationMtController`, `MtService`, AI API MT options, batch
  item/request types including optional ImageBubble image payloads, per-run worker thread
  lifecycle, cancellation, chat context pruning, JSON response parsing, and backend dispatch.
- `machine_translators/`: UI-agnostic Google, Yandex, and DeepL MT provider implementations behind
  `MachineTranslatorBackend`.
- `panels/`: side-panel UI modules for OCR, bubble cards/footer fields, machine translation,
  text detector controls, composed text, and OCR language catalogs.
- `adv_rec.rs`: floating advanced-recognition crop preview/editor with async crop preparation,
  brush overlay, rotation, zoom, and local quick-selection OCR actions.
- `backend_health.rs`: push-driven backend health via `TOPIC_HEALTH` v2 events (with a one-shot
  `health` pull as the startup/liveness fallback), backend version snapshot, plus `device`
  get/set, ONNX provider, max-loaded-models, and CUDA diagnostics helpers.

## Contracts and invariants
- OCR, detector, MT, storage load/save, crop preparation, and backend health work must not block
  the GUI thread.
- Canvas integration must stay behind `CanvasHooks` or typed `CanvasView` APIs; do not duplicate
  canvas bubble/viewport state machines in translation code.
- `draw_canvas_overlay_top_left` must reach its dock block on EVERY frame this tab is active: the
  dock's detached OS windows are immediate viewports and exist only while `PanelDock::end` shows
  them, so an early return above it would close the user's windows for that frame.
- The default dock layout is the DICTIONARY of this tab's dock tabs: a `TabId` missing from
  `translation_default_dock_layout` is dropped from the user's stored arrangement on every load, so
  adding a dock tab here means adding it to that builder too. It is registered under
  `AppTab::Translation.key()` in `app.rs::restore_panel_dock` as well; a builder wired in only one
  of the two places silently resets the stored arrangement.
- Text detector page results carry source size, block rectangles in source pixels, mask size, and
  mask alpha bytes. Mask buffers must match `width * height` and invalid geometry must be rejected.
- Text-detector mask GPU eviction must drop only tiled `TextureHandle` pages and preserve detector
  result data, text-mask model data, and pending storage jobs.
- When mirroring a detection into `TextMaskModel`, the tab passes the detector boxes that match the
  stored mask: `sync_text_mask_page_from_result` stores the RAW backend glyph mask and so passes the
  RAW `result.blocks`; `materialize_text_mask_page_from_blocks_if_missing` rasterizes the RESOLVED
  (expanded/merged) blocks and so passes those resolved blocks. `detector_rects_to_blocks_px`
  converts float `TextDetectorRect`s to integer source-pixel `[x1,y1,x2,y2]` covering rects; boxes
  stay in source-page pixel space (the model is responsible for no scaling).
- Persisted detector results live under `ProjectPaths::text_detection_dir`; storage jobs must load
  and save real block/mask data rather than synthesizing success.
- Footer metadata and ImageBubble metadata live in `Bubble.extra` fields and are synchronized
  through debounced `CanvasView::patch_bubble_extra_fields` calls. Image bubbles do not expose the
  translation character autocomplete; their panel controls edit source type, image path/crop page,
  description, original text, and translation. Translation owns ImageBubble shortcuts: `Q` creates
  an empty external-image bubble under the pointer, and `Shift+Q` captures a drag rectangle as the
  page crop for the selected ImageBubble or creates one at the crop center. External ImageBubble
  files are written to the chapter unsaved staging `image_bubbles/` directory and stored as
  chapter-relative paths so the saved chapter can resolve them after commit.
- Translation also owns the hint-bubble shortcut: `H` creates a `BubbleClass::Hint` under the
  pointer. Unlike `Q`/`Shift+Q` it is a REGISTERED hotkey
  (`HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE`, `app.rs`, default `Key::H` with no modifiers, scope
  `Tab(Translation)`), so it appears in Settings → Hotkeys, is rebindable, and inherits the
  rising-edge semantics of the hotkey manager instead of a hand-rolled armed gate.
- HINT ATTACHMENT is the reason a hint exists, and it is a contract of this tab, not a formatting
  detail. A hint is an author note addressed to the translator: it carries ONE line in
  `Bubble.text`, points at an anchor and owns an area exactly like an ordinary aside text bubble
  (canvas geometry, `src/canvas/MODULE_README.md`), and its only job is to be injected into the
  composed prompt next to the replica it comments on. Binding is FORWARD, in composition sort
  order (`img_v`, or the replica-order spin box the hint shares with every other class): a hint
  attaches to the next ORDINARY replica after it, skipping image bubbles and other hints. Two
  consequences that must not be re-derived by hand: (1) the authoring rule is "place a hint ABOVE
  the replica it annotates" — a hint below its replica binds to the wrong one or becomes trailing;
  (2) a hint is emitted only if its target replica is emitted, so a hint in front of an
  already-translated (filtered) replica emits NOTHING, while a hint with no following replica at
  all is trailing and is force-emitted at the very end past the character limit. That asymmetry is
  deliberate. The full rule — the barrier model, `classify_hint_bindings`, the atomic
  hint+replica limit admission, and the `merge_same_character` interaction — is documented once,
  in `panels/MODULE_README.md`; do not restate it elsewhere.
- `build_bubble_footer` is a class dispatch placed AFTER the shared replica-order spin box, so every
  bubble class inherits the order control: `Image` gets the image source/crop controls, `Hint` gets
  only the `hint_show_outside_translation` checkbox, and `Text` gets the character autocomplete.
  A checkbox/combo in the footer must read a still-pending `pending_footer_patches` entry before
  falling back to `Bubble.extra`, because the patch is debounced and the model lags a few frames.
  `bubble_status_style` exempts `BubbleClass::Hint`: a hint has no original, translation, or
  character, so the incomplete-replica rules would border every hint. `canvas_scrollbar_marks`
  carries the same exemption for the same reason — a hint's single line lives in `text`, so a
  translation-status stripe for it would always read "translated".
- The character list (`character_names`) is ASSEMBLED, not loaded. It is
  `build_translation_character_names(project_roster, chapter_names)` and its order IS the suggestion
  priority: the "no character" sentinel, the `characters.json` roster
  (`character_names_project`), the free-form names actually typed on this chapter's bubbles
  (`character_names_chapter`), then the built-in service names — trimmed, deduplicated
  case-insensitively, first spelling kept.
- Its two consumers do NOT get the same slice, and that difference is a correctness contract, not
  cosmetics. The bubbles panel takes the list whole: its character control is a combo box, the
  sentinel is a selectable row and `character_names[0]` is the panel's fallback value. The footer
  takes `footer_autocomplete_candidates(&character_names)` — the same slice with a LEADING
  `footer_no_character()` removed (conditionally, never a blind `[1..]`) — because its control is a
  free-text `AutocompleteLine` that splices the tail of a candidate from a word boundary: with
  "(не указан)" among the candidates, typing "ук" writes the fragment `указан)` into the field with
  `changed = true`, and one debounce later that fragment is a persisted character name no sentinel
  filter can recognize. Anything added to the assembled list must be safe to splice tails from.
  Both consumers borrow — the list stays ONE `Vec<String>` and is never cloned per frame.
- `collect_chapter_character_names` reads the whole-chapter bubble snapshot with the same
  `extra["character_name"]` reader the footer state uses, ignores `is_known_character`, and drops
  blanks, the `footer_no_character()` / `footer_no_characters()` sentinels, and the `Image`/`Hint`
  classes (neither owns a character field). The sentinel filter is REQUIRED: `panels/bubbles.rs`
  persists `footer_no_character()` into `Bubble.extra` when the "И.П." checkbox is switched on for
  a bubble whose name is not in the list.
- The chapter scan is kept off the per-frame path by `sync_footer_tracking`'s
  `hook_bubbles_revision` gate and reuses the snapshot already taken there; inside the gate the
  scan always runs and only the reassembly is skipped when the collected set is unchanged. The
  revision moves on every footer patch flush (~`FOOTER_PATCH_DEBOUNCE_SECS` while a name is being
  typed), so that scan must stay linear and allocation-light. The same debounce is why a name
  typed in the footer joins the list one flush later rather than on the keystroke. The chapter
  scan reads the runtime-inclusive snapshot while `collect_recent_character_history` beside it
  reads `project.bubbles`; the asymmetry is deliberate and documented at the call site.
- Post-OCR processing is ONE helper, `ocr.rs::apply_post_ocr_processing`, and all three
  recognize routes (AI API, native ONNX, backend IPC) must call it — engine parity of the
  published text is a contract of this module. The caps-lock decision and the caps-lock
  rewrite straddle the user's `CharReplacementRule`s: `ocr_case_fix::looks_like_caps_lock`
  runs on the RAW engine output (a repair rule such as `"0" -> "o"` must not be able to
  introduce a lowercase letter and veto the fix), the substitutions run next, and
  `ocr_case_fix::apply_caps_lock_fix` — which is unconditional and trusts the caller's
  gate — runs last so it sees the final punctuation (the default `…` -> `...` rule creates
  real sentence terminators). The detector accepts a result only when the whole of it (both
  `text` and `lines`) holds at least one uppercase Latin/Cyrillic letter and no lowercase
  one. Characters outside those two scripts never take part in the decision and are copied
  through unchanged, so byte-identity is guaranteed only for a result with no
  Latin/Cyrillic letters at all or one the detector rejected — `"HELLO 日本"` becomes
  `"Hello 日本"`. The rewrite keeps only the first letter of a stream and the first letter
  after `.`/`!`/`?`/`…` uppercase, with ONE exception: the English pronoun `I` and its
  contractions (`I'm`, `I'll`, `I've`, `I'd`, either apostrophe) stay capitalized anywhere.
  That exception is a whole-word match applied as a second pass per fragment, so `is`, `in`
  and `i18n` are untouched and it re-cases exactly one ASCII character. The rewrite threads
  one sentence state across the elements of `lines` so a sentence continues over a line
  break, but rewrites each element in place: `lines.len()` is preserved structurally, and an
  element that already contains a `\n` is never split.
- App-managed OCR/detector model downloads must go through `src/ai_models.rs`. Missing models,
  backend failures, unavailable credential stores, AI API request failures, and unsupported engines
  must surface clear errors.
- MT worker events are accepted only for the active run id; cancelled/detached run output must not
  mutate canvas state. AI API MT responses must be matched by bubble ID before applying text; for
  ImageBubble results, original text and translation are applied together.
- A failed AI run whose error matches `is_probable_quota_or_limit_error` (keyword/HTTP-code scan)
  stops quietly: instead of a red error toast the panel shows the sticky `MtStopNotice` with the
  full provider error available behind a toggle. Other run failures keep the red toast.
- `build_ai_mt_request_preview` assembles the first AI request (system prompt + first batch user
  message with decoded inline images) without contacting the provider, reusing the exact item sort,
  batch split, and `build_ai_mt_user_parts` ordering as a real run. `tab.rs` runs it on a worker
  thread (it loads/decodes images) and shows the result in the debug "Полный запрос" window; the
  per-button right-click action only applies to the AI API tab.
- The AI MT worker emits diagnostic lines to `runtime_log` (session `last.log`) tagged
  `[MT][run <id>]`: a run-start summary (service, model, batch counts, translate/context/total item
  counts, image-bubble count, image detail, context budget), one line per batch before the request
  (replica/translate/context counts, attached image count and KiB, history depth, pruned total) and
  one after the response (response length, parsed entry count or parse error), plus the per-batch
  and final translated/errors totals. Raw provider request errors are logged before being wrapped
  into `RunFailed`. Logging must never include API keys or image binary contents.
- Shared model locks must be short-lived and released before image decoding, HTTP calls,
  composition/export, storage I/O, or callbacks.

## Editing map
- To change top-level translation UI routing, canvas overlays/hooks, OCR selection behavior,
  detector storage, footer sync, or settings persistence, edit `tab.rs`.
- To change which dock tabs this program tab declares, where its panels start, or what the
  text-detector edit boxes anchor to, edit `draw_panel_dock` / `text_detector_edit_panel_pos` /
  `translation_default_dock_layout` in `tab.rs`; the «Лента» tab's own content, sizes, title and
  declaration (`canvas::declare_ribbon_tab`) live in `src/canvas/`.
- To change what the recent-character cards LOOK like or how they wrap, edit
  `draw_recent_characters_tab_body` in `tab.rs`; to change how big their panel starts or how small
  it may get, edit the two `TRANSLATION_RECENT_CHARACTERS_TAB_*_SIZE_PX` constants there. The
  history itself is `collect_recent_character_history` and the digit lookup is
  `recent_character_entry_for_rank`.
- To change what the character list holds or in what order, edit
  `build_translation_character_names` (group order/dedup) and `collect_chapter_character_names`
  (which bubbles contribute a name) in `tab.rs`; to change WHEN it is rebuilt, edit
  `sync_chapter_character_names` and its call site inside `sync_footer_tracking`; to change what
  the FOOTER field is allowed to suggest (as opposed to the panel combo box), edit
  `footer_autocomplete_candidates`.
- To change OCR engine options, loading, recognition requests, IPC method names, crop handling, or
  page image caching, edit `ocr.rs` and the OCR panel in `panels/ocr.rs`.
- To change text detector algorithms, masks, backend IPC methods, or cleaning-tool detector
  helpers, edit `text_detector.rs` and `panels/text_detector.rs`.
- To change MT run lifecycle, provider selection, cancellation, or batch dispatch, edit
  `machine_translation.rs`, `machine_translators/`, and `panels/machine_translation.rs`.
- To change bubble list editing, footer fields, search/filter behavior, or text write-through, edit
  `panels/bubbles.rs` and footer sync helpers in `tab.rs`.
- To change composed text generation or export, edit `panels/composition.rs`.
- To change advanced manual OCR crop behavior, edit `adv_rec.rs`; keep OCR dispatch in `tab.rs`.
