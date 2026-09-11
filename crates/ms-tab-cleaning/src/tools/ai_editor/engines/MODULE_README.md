# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines

## Purpose
The AI engines the «ИИ-редактор области» tool hosts, one module per engine, each implementing
`super::engine::AiEngine`. An engine owns everything model-specific — its parameters and their
persistence, its wire protocol, its worker threads, its own progress bar — and nothing about the
canvas: the rectangle, the painted mask, the pending result and the apply path belong to the host
(`../MODULE_README.md`) and the frame (`../../region_edit_v2/MODULE_README.md`).

An engine is NOT a `CleaningTool`. It has no selection, no editor window, no canvas hooks and no
entry in `tab.rs`; it reaches the user only through the host's panels.

## Architecture
```
mod.rs::all_engines()      -> one instance of every engine, in picker order
AiEngine::constraints()    -> the frame's size rules
AiEngine::mask_layers()    -> the frame's mask stack (tint + name key per layer)
AiEngine::draw_parameters  -> the body of the left «Редактор области» panel
AiEngine::start(request)   -> region + masks in, worker thread out
AiEngine::poll(ctx)        -> Idle / Running / Done(ColorImage) / Failed(message)
```
Adding an engine is a module plus one line in `all_engines`. The picker's two sections come from
`AiEngine::section`, not from the order of that list.

ONE MODULE MAY CONTRIBUTE SEVERAL ENTRIES when the difference between them is a parameter and not
an implementation. `flux2_klein/` contributes one per `config::Flux2Variant` — FLUX.2 klein 9B and
FLUX.2 klein 4B — and the variant keys the engine id, the picker caption, the settings FILE, the
model directory and the `variant` field the two `.download.*` methods carry. Duplicating the module
for a second checkpoint is the wrong answer; parameterizing it is the pattern.

THE OPPOSITE SHAPE IS ALSO CORRECT, and `lama/` and `sdxl/` are it: ONE picker entry offering four
checkpoints behind TWO backend methods (`lama/`), and ONE picker entry offering two channel modes
behind ONE backend method (`sdxl/`), chosen inside the engine's own panel. The line between the two
shapes is what the user is choosing. A FLUX.2 klein variant changes what the engine IS — its model
directory, its settings file, its download — so it is a picker entry. A LaMa checkpoint or an SDXL
channel mode changes only how one engine runs, so it is a parameter, and folding them into one entry
each is what keeps the sections from being lists of near-identical buttons.

An engine's panel body owns its own structure, and FLUX.2 klein's is the reference for a big one:
prompt block (field, the one cache line, the «Перевод» / «Библиотека промптов» toggles) → the one
creative dial → the ONE readiness line with its «Установить» / «Обновить» buttons → three SIBLING
collapsible sections («Установка модели», «Память и скорость», «Для экспертов») → the mask note.
The rule the layout follows: what is touched per edit is never behind a fold, what is set once per
machine always is, and no section wraps or nests another. The decisions the lines report are pure
functions with unit tests, not conditionals inside the drawing code.

## Files and submodules
- `mod.rs`: the catalog — `all_engines()`, plus the one helper every engine's run path shares
  (`region_size_refusal`, the `FrameConstraints` re-check described below). It also decides ONE
  thing beyond the list: the engine at index 0 is the one selected when the tool is created, which
  is why FLUX.2 klein 9B stays at the head and `lama` then `sdxl` are appended after it. Screen
  order is unaffected — the picker draws «Без промпта» before «С промптом», so «Lama» appears first
  regardless.
- `flux2_klein/`: the FLUX.2 klein engine — a DIRECTORY, because it outgrew one file, and the
  source of TWO picker entries (9B and 4B, one per `config::Flux2Variant`); its own
  `MODULE_README.md` is the map of the split and is where to look before editing it (IPC methods
  `inpaint.flux2_klein` streaming,
  `.status`, `.estimate`, `.unload`, `.component_action` streaming, the six
  `.prompt_cache.*` methods, and the two `.download.*` methods). The user paints the
  area the model is ALLOWED to change, writes a prompt and gets that area regenerated; everything
  outside the painted mask must survive untouched, which is why the engine declares exactly ONE
  mask layer and puts its bytes on the wire verbatim. Leaving that layer EMPTY is the other
  working mode — the whole region is regenerated — and it is derived, not chosen; see the
  contracts below. Size contract:
  multiple of 16, shortest side >= 128 px, area <= 1 MP, aspect not steeper than 8:1, declared
  through `constraints()` AND re-checked against the actual region size on the run path. The prompt
  field is doubled: an optional user-language field with a «Перевести ↑» button on its row, plus a
  Google/Yandex/DeepL picker, which together fill in the ENGLISH field that is the only one sent, reusing the translation tab's own
  `translate_texts_via_translator` on a worker thread. That English field is never empty:
  `FLUX2_DEFAULT_PROMPT` is substituted both for a new settings file and for one whose prompt is
  missing or blank, because an empty prompt blocks the run gate. Under it sits ONE compact line
  about the PROMPT CACHE: `.status` answers `prompt_cached` for the prompt it was asked about (an
  optional field — an absent one reads as "not known", never as "not cached"), and the line is shown
  only while that answer still describes the prompt in the field. Which of the competing facts takes
  that single line is the pure `flux2_prompt_cache_line`, and an unknown state draws NOTHING — an
  outstanding query is not information. The LIBRARY that produces the caches is folded behind the
  «Библиотека промптов» toggle beside the prompt (session state, never a setting), and folding it
  away changes no gate. «Кэшировать» runs the streaming `.prompt_cache.build` on the SAME progress bar as a
  generation (so neither can start while the other runs — reading the Qwen3 encoder off disk
  costs tens of seconds, which a cached prompt skips entirely). The saved caches form a LIBRARY that lives
  backend-side (`prompt_cache/`, one folder per encoder family): `.prompt_cache.list` fills a
  `WheelComboBox` of named entries, `.save`/`.load` take a NAME (typed in an inline field beside
  the button, following the watermark library and the typing presets rather than a modal of its
  own), and `.export`/`.import` carry one entry through a `.msprompt` file with the same
  non-blocking picker the model paths use. An imported file of a foreign family is stored under
  that family and reported as such; the engine shows a warning, because such an entry never appears
  in this family's list and the backend refuses to load it. `.status` and `.prompt_cache.list` are
  re-armed through the same one-shot flags as the memory forecast, so editing the prompt cannot
  turn a keystroke into a request. GENERATION WITHOUT A LOCAL TEXT ENCODER is supported — see the
  contracts below. Model paths (Qwen3 text encoder folder, transformer file or diffusers folder,
  VAE) are entered by hand or through a non-blocking native picker and persist to the VARIANT's own
  settings file (`config::flux2_klein_settings_path`; 9B keeps `flux2_klein_settings.json`
  unchanged, 4B gets `flux2_klein_4b_settings.json`). Memory placement is chosen
  through four BUILT-IN presets («Максимум скорости» / «Сбалансированный» / «Минимум RAM» /
  «Минимум VRAM»); «Пользовательский» is never selectable — it is what the picker reports when the
  SEVEN fields a preset owns match none of them (placement, `low_cpu_mem_usage`, VAE
  tiling/slicing, `unload_transformer_before_vae`, `unload_text_encoder_after_encode`,
  `text_encoder_fp8`). The last two are the text-encoder memory controls, and BOTH are `false` in
  EVERY preset. The Qwen3 encoder is ~16 GB and is needed exactly once per generation, but it is
  loaded LAST — after the transformer already sits on the card — so it occupies host memory the
  pipeline has just vacated, and holding it turns a new prompt from a fresh read of the weights
  into a single encode pass; that is why no preset unloads it after encoding. What reaches memory is
  LESS than what sits on disk: the encoder is loaded as a `Qwen3Model` truncated to the decoder
  layers the pipeline actually reads, which on the shipped 4B encoder leaves about a fifth of its
  tensors unread, so `status.components[*].size_bytes` (disk) and the memory forecast (RAM)
  deliberately disagree for this one component. `text_encoder_fp8` stays off because quantizing
  costs embedding quality and is the user's decision alone. Whether a negative prompt exists is
  decided by the CHECKPOINT, not assumed: the backend reads `is_distilled` from `model_index.json`
  and reports `guidance_supported` in `.status` (absent means supported). Both shipped klein
  checkpoints are distilled, so guidance is inert there, the negative prompt is never encoded, and
  the UI greys the control out (4 steps, guidance 1.0). The RAM/VRAM forecast is
  COMPUTED BY THE BACKEND (`.estimate`, peak = max over PHASES: prompt encoding, denoise, VAE
  decode); this side only formats it and warns when `fits` is false. The `breakdown` peaks are
  looked up by name and each one is optional, so a backend that does not report a phase simply
  loses that line of the tooltip. `.status` and `.estimate` both carry the normalized `params`: the
  backend answers about the paths in the REQUEST, falling back to those of its last successful
  generation when they are absent, so a query without them reports "nothing is configured" for the
  paths the user has just entered. The single progress bar is shared by every run of the engine and
  is claimed by GENERATION, so a cancelled run cannot move or clear the bar of the run that
  replaced it; cancelling also stops the backend through `CallHandle::cancel` instead of only
  dropping the answer.

- `lama/`: the «Lama» engine — a DIRECTORY for the same reason `flux2_klein/` is one, and the source
  of exactly ONE picker entry offering FOUR checkpoints. Its own `MODULE_README.md` is the map. The
  user paints what must be REMOVED and gets the background restored under it; there is no prompt and
  no creative dial, so it sits in the «Без промпта» section. IPC methods `inpaint.lama_v2` /
  `.unload` and `inpaint.lama_mpe` / `.unload`, both plain request/response (no streaming, therefore
  no progress bar and no backend-side cancel). Size contract: multiple of 8, shortest side >= 8 px,
  no area and no aspect limit. Its three engine-specific contracts are in the section below: the
  mandatory mask, the per-entry method dispatch and the refine rule.

- `sdxl/`: the «SDXL Inpaint» engine — a DIRECTORY for the same reason, and the source of exactly
  ONE picker entry offering TWO channel modes. Its own `MODULE_README.md` is the map. The user
  paints what must be REGENERATED, writes a positive and a negative prompt and gets that area
  redrawn, so it sits in the «С промптом» section beside FLUX.2 klein. IPC methods `inpaint.sdxl`
  (STREAMING — one `progress` frame per diffusion step, carrying `step` / `total` and an optional
  latent preview PNG in the frame blob) and `inpaint.sdxl.unload`. Size contract: multiple of 8
  (the SDXL VAE's downscale factor), shortest side >= 8 px, no area and no aspect limit. The two
  modes are
  `nine_channel` (a dedicated 9-channel inpainting UNet, full denoise, no prefill) and
  `four_channel` (an ordinary SDXL checkpoint over a LaMa prefill, moderate denoise); each owns a
  COMPLETE parameter set, both are persisted, and only the 4-channel one puts `lama_model` on the
  wire. There is no model catalog and no presence scan: the checkpoint is a path or a Hugging Face
  repo id the user types, which is why the run gate closes on an empty one. Its engine-specific
  contracts are in the section below: the mandatory mask, the per-mode parameter sets and the
  progress generation.

## Contracts and invariants
- **An engine never touches the canvas.** No `CanvasView`, no `ProjectData`, no frame, and no
  `egui::Context` outside `poll`. Its whole UI surface is a plain `&mut Ui`, and its answer is a
  `ColorImage` of exactly the frame rectangle — merging it is the host's job.
- **The size guarantees of `EngineRunRequest` are checked, not trusted.** `start` refuses a region
  or a mask buffer that does not match `rect_px`: the user gets the localized "invalid region size"
  line and the log gets the page index, the rectangle and every buffer length.
- **Nothing blocks the GUI thread.** Settings I/O, every IPC call, the native file pickers, the
  prompt translation and even the one-frame cancel write run on `ms_thread::spawn` workers; `poll`
  only drains channels. `poll` is called every frame whether the parameter panel is drawn or not,
  so a finished run lands with the panel closed.
- **`poll` is also the only writer of the settings file, and losing it loses data silently.**
  Every engine's saver runs inside `poll`; a host that stops polling keeps the tool working and
  quietly discards every model path, memory preset, prompt and parameter on exit. The arming rule
  is the pure `settings_save_due(dirty, settings_loaded, save_in_flight)` — a plain gate with no
  time debounce, so a save starts on the first poll it is due on: a write before the initial load
  lands would overwrite the user's file with the in-memory defaults, so `dirty` is kept pending
  instead of dropped, and at most one writer touches the path at a time.
- **A user edit outranks a settings load that lands after it** (`lama/`, `sdxl/`). The host draws an
  engine's panel body earlier in a frame than it polls that engine, so a value changed before the
  load landed would be replaced silently; `poll_settings_load` therefore applies the file only while
  `dirty` is clear, and drops the whole loaded document otherwise.
- **The run path re-checks the size against `constraints()`, it does not trust the host.** The frame
  snaps and validates a rectangle against the same constraints, but the rectangle and the region an
  engine is handed can disagree, so `AiEngine::start` refuses a violating size instead of encoding
  it onto the wire. `lama/` and `sdxl/` do it through the shared `region_size_refusal` in `mod.rs`,
  which reads `region_edit_v2::geometry::check_size` — the one authority on what a valid size is —
  and answers in the host's own violation wording; `flux2_klein/` does it through its own
  `region_block_reason`, whose message set is engine-specific and whose agreement with `check_size`
  is pinned by a unit test.
- **The RAM/VRAM forecast is armed by a SETTLED SIZE change and by nothing else geometric.**
  `set_region` is pushed every frame and carries `geometry_settled`; the rectangle is stored
  unconditionally, so the status line keeps printing the size the user is dragging, but the
  `.estimate` round trip is armed only when the region's SIZE changed AND no frame gesture is in
  flight. A pure position change costs nothing — scrolling moves the rectangle through the frame's
  keep-in-view clamp — and a resize drag costs exactly one request, on release, rather than one per
  rendered frame. The size change is held in `region_resize_pending` because the rectangle usually
  does not change on the release frame itself. Everything that is NOT the geometry (a parameter,
  the memory preset, a loaded prompt cache, a finished run, the settings load) still arms the
  forecast directly, and a test pins that.
- FLUX.2 klein's `whole_region` is a WORKING MODE that is DERIVED FROM THE PAINTED MASK and is not a
  setting: there is no switch, no persisted field and no `MemoryPresetValues` entry for it, and
  `mask_for_run` is the ONLY place the decision is made — it returns the wire buffer and the flag
  together, so the two can never disagree. An empty layer means "edit the whole region" and yields a
  SOLID mask (every byte `255`) with `whole_region = true`, because the backend refuses that flag
  unless the mask really is uniform; anything painted means "edit only what is under it" and travels
  verbatim with `whole_region = false`. The threshold is "any byte above zero": one painted pixel is
  already a mask, so a stray dot can never become permission to regenerate everything. The host's
  painted layer is never overwritten — the solid buffer is built BESIDE it — and `allows_empty_mask()`
  is therefore unconditionally `true`, which is what stops the frame demanding a non-empty mask. It
  is the HOST that tells the user an empty mask is legal (`AiEditorTool::draw_empty_mask_hint`), not
  this engine. `whole_region` also travels as `false` on every path that only ASKS the backend
  something (`.status`, `.estimate`, the prompt-cache calls): no mask exists there, which is why
  `to_params` takes the flag instead of reading it. Backend-side `mask_dilate_px` is ignored when
  nothing is painted; the slider is not faded for that (the engine cannot see the host's mask
  stack, so the state is unknowable while the panel is drawn) and its hover text names the condition
  instead. `mask_feather_px` keeps working either way and softens the join between the region and the
  page.
- **Lama's mask is MANDATORY and means the OPPOSITE of FLUX.2 klein's.** `allows_empty_mask()` is
  unconditionally `false`: the mask says WHAT TO REMOVE, so an empty one describes no work at all,
  and the host disables «Обработать» rather than sending a request whose only possible answer is the
  region it was given. Nothing in the panel can change that answer, so the host's per-frame re-read
  simply keeps agreeing with it, and `draw_empty_mask_hint` draws nothing. The single layer carries
  the inpaint yellow the mask-inpaint editor uses (`../../base.rs`), because the engines share one
  frame and one brush and the colour is what tells the user which meaning is in force.
- **A Lama catalog entry carries its METHOD and its refine support as constants; nothing re-derives
  them.** `inpaint.lama_v2` and `inpaint.lama_mpe` are different architectures — different
  generator, different forward arity, different weight layout, different model directory — so the
  backend keeps both methods and `LamaModelSpec::method` dispatches. The user still sees one run
  button and one unload button; which method they reach is the selected entry's business. The
  parameter panel follows the same field: a v2 entry draws refine plus `n_iters` / `max_scales` /
  `px_budget`, the MPE entry draws `inpaint_size`, and neither shows a parameter its method ignores.
  Both methods are the backend's contract and travel under `PROTOCOL_VERSION`: changing either
  request shape is a protocol change, not an engine-local one.
- **`refine: true` can only ever reach the wire for an entry that declares `supports_refine`.** The
  backend refuses its refine pass on a TorchScript `.pt` outright and LaMa-MPE has no refine pass at
  all, so a leftover checkbox would turn into a failed run. The flag is decided in exactly one
  place, `effective_refine(spec, settings)`, which the header builder itself calls — no call site can
  bypass it — and the UI closes the control with a tooltip that says why instead of leaving it
  merely grey. It is a CONSTANT of the catalog entry, never re-guessed from the file extension: a
  second copy of that rule at a call site is a copy that drifts.
- **The Lama model catalog is the only cross-engine dependency in this subtree, and it is
  one-directional.** `sdxl/` reads its LaMa-v2 VIEW (`lama_v2_model_catalog`) for the 4-channel
  prefill picker, which is why those items are `pub(in crate::tabs::cleaning::tools::ai_editor::engines)`
  rather than private to `lama/`. Nothing outside `engines` sees them, and `lama/` knows nothing
  about `sdxl/`. That view exists so the MPE entry can never be offered there: the SDXL request
  sends a checkpoint FILE NAME the backend resolves inside `Torch/LaMa/models`, a directory the MPE
  checkpoint is not in, and `ensure_lama_model_for_external` refuses any name outside the v2 subset
  for the same reason. Duplicating the catalog on the SDXL side is the wrong answer — a second copy
  is a copy that drifts.
- **SDXL's mask is MANDATORY too, and means REGENERATE.** `allows_empty_mask()` is unconditionally
  `false`: SDXL inpainting has no whole-region mode the way FLUX.2 klein does, so an empty mask
  describes no work at all. The single layer carries the same inpaint yellow `lama/` uses, because
  the meaning is the inpainting hole in both and the colour is what tells the user which meaning is
  in force.
- **An SDXL channel mode owns a COMPLETE parameter set, and both are persisted.** Switching mode
  switches the whole set; it never merges the two, and the mode that is not selected keeps its
  prompts, its weights path and its denoise. That is why the persisted document holds both sets
  plus the selected mode, in ONE file — a file per mode could not record which mode to restore.
  `lama_model` is read only by `four_channel` and reaches the wire only there
  (`lama_model_for_run`): the 9-channel pipeline has no prefill step and would not know the field.
- **SDXL's progress bar is claimed by GENERATION.** `inpaint.sdxl` streams a frame per diffusion
  step, and a cancelled run is DETACHED — `call_streaming` never hands out the request id
  `Client::cancel` would need, so the backend finishes the pass. Every publication into the shared
  progress is therefore stamped with the generation that
  claimed the bar, so an abandoned worker can neither move nor clear the bar of the run that
  replaced it. The engine has no other cancellation, and the run cannot be stopped backend-side.
- **The SDXL settings are NOT clamped on load, unlike Lama's.** Persisted values travel to the wire
  verbatim and two of the fields are free-form text (a weights path and two prompts); a clamp on
  load would silently rewrite a value a user put in the file by hand. The panel's own ranges bound
  everything entered through the UI, which is the only path that matters in practice.
- A settings file written by an older build carries keys that map to no field any more:
  `whole_region` (the mode is derived from the mask), `max_sequence_length` (PINNED to
  `FLUX2_MAX_SEQ` = 512 rather than a setting — 512 is the maximum, so every value a user could
  pick is a reduction, and the length is part of the prompt-cache key, so lowering it invalidates
  the whole saved `.msprompt` library at once) and `brush_radius` (the brush belongs to
  the host's `MaskBrush`). `Flux2KleinSettings` does NOT use `deny_unknown_fields`, so serde drops
  them and the document loads with every other setting intact; no migration exists for any of them
  and none may be added. A test pins that such a document loads identically to the same document
  without the keys.
- FLUX.2 klein RUNS WITHOUT A LOCAL TEXT ENCODER when the prompt is already cached: the denoise and
  the VAE decode never read the encoder, so a `.msprompt` carried to a machine that never downloaded
  the 16 GB Qwen3 is enough. The run gate (`flux2_run_block_reason`) therefore waives the
  `text_encoder_path` requirement on `prompt_cached == Some(true)` and ONLY on that — `None` is "not
  known" (or a backend too old to report the field, which could not generate without an encoder
  anyway). The transformer, the VAE, the tokenizer and the scheduler are never waived, and the
  backend keeps the final say (`_first_unavailable_reason`); this gate exists to explain a refusal
  before the click, never to duplicate the decision. Three optional `.status`/`.prompt_cache` fields
  carry the state, all parsed as three-state `Option<bool>` where an absent field means "not known"
  and never `false`: `.status.text_encoder_available` (an empty path and a path that does not exist
  are the same `false` — the second is what a settings file copied from another machine looks like),
  `.prompt_cache.load.encoder_verified` (whether the encoder fingerprint was compared or the file's
  metadata was taken on trust) and `.prompt_cache.list.text_encoder_available`. The UI consequences:
  the one line under the prompt taking an amber warning (a warning, not an error — ready caches
  still work) unless the prompt is already cached, in which case the affirmative wins because the
  run works either way,
  «Кэшировать» and «Сохранить кэш» disabled with their own tooltip (they are the only two library
  operations that need the encoder, and the backend refuses both), «Загрузить»/«Экспорт»/«Импорт»
  untouched, and a ONE-OFF notice in the prompt-cache warning slot after a load whose
  `encoder_verified` was `false`. With no encoder there is no ACTIVE family, so `.prompt_cache.list`
  answers an EMPTY top-level `family` and lists every family at once; each entry then carries its
  own `family` and the combo shows it as `<family> / <name>`. That label is DISPLAY-ONLY — the wire
  identifies an entry by NAME alone, and a name present in two families is a backend error rather
  than an arbitrary choice.
- FLUX.2 klein's request blob is `region.png ++ mask.png` with the mask an L8 PNG of EXACTLY the
  region size; the response header's `image_len` is validated with STRICT equality against the blob
  length and the decoded PNG must be exactly the region size. The response also carries
  `oom_recovered` and an `applied` object of FIVE memory flags (`unload_transformer_before_vae`,
  `vae_tiling`, `vae_slicing`, `unload_text_encoder_after_encode`, `text_encoder_fp8`): when the
  backend recovers from an out-of-memory failure during the VAE decode it retries the decode with
  cheaper flags, and the engine writes those flags back into its settings (so the next run starts
  economical) and says so in its status line. A partial `applied` object — including one carrying
  only the three older flags — is ignored wholesale rather than half-applied.
- The two placement-derived settings flags (`unload_transformer_before_vae`,
  `unload_text_encoder_after_encode`) default from the PLACEMENT, which serde's per-field default
  cannot see. `settings_from_json` derives them for a settings file written before the field
  existed — `false` under `full_gpu`, `true` for every economical placement — and leaves a file that
  carries them untouched. Add any further placement-dependent flag there, not in a serde default.
- The size gate is SPLIT and must stay split: the frame validates a rectangle against
  `constraints()`, the run path re-validates the actual region against `region_block_reason`, and
  `flux2_run_block_reason` answers only for the model paths, their CONTENTS and the prompt. A test
  pins that `constraints()` and `region_block_reason` accept exactly the same regions.
- **Whether the model is installed is ONE answer, `flux2_model_readiness`**, derived from the
  cached `.status` presence catalog together with `effective_paths` and the source mode. It is
  three-state and the three states are not interchangeable: `Unknown` (no answer has landed yet)
  must NEVER block a run — an unanswerable status is not evidence of a missing model, and blocking
  on it leaves «Обработать» dead on a slow backend; `Missing` is the only blocking verdict and
  carries the SET of missing components, so one refusal names them all instead of sending the user
  round the loop once per file; `Ready` passes. An empty required path is decided locally without
  the backend, everything else waits for the catalog. This function exists because the emptiness
  check alone CANNOT see a fresh download-mode installation: `effective_paths` derives its three
  paths under `config::flux2_klein_dir()`, so they are never empty; without the catalog the user
  gets the backend's untranslated «Путь transformer_path не найден» instead of a local, actionable
  refusal.
  The refusal text differs by mode because the fix does — a download is a button, a wrong manual
  path is a field. The catalog is an answer ABOUT THREE PATHS, so it counts only while it still
  describes the ones in the settings: `flux2_status_for_paths` drops an answer fetched for others,
  which turns the verdict back into `Unknown` rather than leaving a refusal standing over a path
  the user has just corrected, and a path change re-asks (`Flux2KleinEngine::note_settings_changed`
  — a typed path arms the query, not only the folder picker). This is the same guard
  `prompt_cache_state_for` applies to the prompt half of that answer, and every consumer reaches the
  catalog through `Flux2KleinEngine::status_for_current_paths`. The SAME verdict is what the panel's
  always-visible readiness line reports (`flux2_readiness_line`) and what decides the INITIAL open
  state of «Установка модели», so the refusal under «Обработать» and the line above the section can
  never disagree. «Установка модели» starts folded and opens ITSELF once, on the first `Missing`
  that lands (`flux2_install_seed`) — never from a `default_open`, which on the first drawn frame
  would only ever see the `Unknown` that precedes the first `.status` answer, and would therefore
  open the section on every launch. A fold the user has already moved by hand retires that seeding:
  a section closed while the verdict was still `Unknown` must not be reopened by the answer that
  arrives afterwards. `Missing` is the
  only state that offers «Установить», which opens that section; `Unknown` is neutral and offers
  nothing, because nothing is known to be wrong. The `Ready` line carries the memory forecast and
  turns amber on `fits: false` — the spelled-out warning lives inside «Память и скорость», which is
  folded by default.
- The engine's own `run_status` line carries only what NO OTHER SURFACE says: finished, recovered
  from an out-of-memory failure, failed, cancelled. "The run has started" is deliberately absent —
  the progress bar directly above it, the host's panel status line and the frame chrome all report
  a running job already. A starting run CLEARS the slot instead, so the previous run's outcome
  cannot linger over a new one.
- FLUX.2 klein reports PER-COMPONENT RESIDENCY and lets the user act on it, in the SAME list
  that reports which parts are on disk — one row per component carrying presence, size, residency
  and the offered buttons together (`flux2_component_rows`), because two adjacent blocks named the
  same five parts twice. A row the residency answer does not cover (the tokenizer and the scheduler
  always; every row while the block is busy or unreported) keeps its presence half and claims no
  state. `.status`
  answers a `components` block — for the text encoder, the transformer and the VAE — carrying
  `residency` (`not_loaded` / `ram` / `gpu` / `offloaded` / `mixed`; five states because
  accelerate's offload leaves the parameters on `meta` with the bytes in a host map, and a load
  can leave a component genuinely split) and `actions`, the list of what may be done to it right
  now. **`actions` is the AUTHORITY and is never re-derived on this side**: the rule depends on
  the accelerate hooks, the pipeline-wide model cache key and the memory guard, so duplicating it
  in Rust would put the matrix in two languages and drift on the first edit. The three-state rule
  the optional `.status` fields already carry applies to the whole block: an ABSENT `components`
  is NOT KNOWN and never "not loaded", with `components_busy` saying the service could not take
  the lock a generation holds for its whole run; while it is busy the block reports that instead
  of stale rows and draws no buttons. A residency literal this build does not know leaves the row
  stateless and shows the literal on hover, and an unknown action is dropped from that row rather
  than guessed at. The transformer and the VAE load and unload TOGETHER — one cache key describes
  a whole pipeline — and their button hovers say so. Acting on a component is the streaming
  `.component_action`, which claims the SAME progress bar as a generation and a
  `.prompt_cache.build`; all three are therefore mutually exclusive, and every "wait for the
  current operation" gate reads `flux2_pipeline_busy` across the three rather than one receiver.
  The pinned wire contract is `dev-docs/flux2_component_residency.md`.
- **The model source is a MODE, and the two modes never mix.** `source_mode` (`manual` /
  `download`) is a persisted field beside the paths, switched by two toggle buttons at the TOP
  of «Установка модели», and exactly one body is drawn under it: the three manual path rows,
  or the download block. It DEFAULTS TO MANUAL and a settings file without the field loads as
  manual — such a file carries hand-entered paths, and loading it as `download` would hide a
  working configuration behind an empty download block and read as an update that broke the
  install.
  - **`Flux2KleinSettings::effective_paths` is THE answer to "which three paths does a run
    use"**, and every consumer goes through it — the wire `params`, the run gate, the
    prompt-cache gates, the read-only paths the download body shows. Two places computing it
    is how the two modes drift apart.
  - **In download mode the three paths are DERIVED** from `config::flux2_klein_dir(variant)` and
    the encoder toggle, and **the manual fields are never read and never written**. A finished
    download therefore does NOT write its answer into the settings: the manual fields hold a
    configuration the user switches back to, so writing over them would destroy it. The answered
    paths are kept only to LOG drift between the two halves. Derivation is what configures the
    engine, continuously rather than once at the end of a download.
  - The derived encoder is a SIBLING of the downloaded `tokenizer/` and `scheduler/` under the
    VARIANT's own root (`FLUX.2-klein-9B/` or `FLUX.2-klein-4B/`), which is what the backend needs: `component_search_roots` probes each
    supplied path AND ITS PARENT, so the repo root is reached from any of the three and both
    `text_encoder/` and `text_encoder_uncensored/` resolve the tokenizer (verified against
    `modules/ai_backend/inpaint/flux2_klein.py`, hit at probe position 2 of 8 for both).
  - The uncensored toggle belongs to the download body alone. In manual mode it is meaningless
    — the user simply points at whichever encoder he wants — and it is drawn only for a variant
    that HAS an uncensored encoder published for it (`supports_uncensored_encoder`, true for 9B
    only). `normalized()` forces the flag off for the other one, so the pairing the backend
    refuses cannot reach a path or the wire even from a hand-edited settings file.
- FLUX.2 klein DOWNLOADS ITS OWN MODEL from Hugging Face, in the download mode of that switch:
  `.download.check` (one-shot) and `.download.start` (streaming). The pinned wire contract is
  `dev-docs/flux2_model_download.md`. What this side owns:
  - **The token is NOT this engine's.** It is the process-wide `crate::hf_token` global, kept in
    the OS secret store under `"ManhwaStudio Hugging Face"`, seeded at startup. This engine is its
    first UI surface, not its owner: the token is never a `Flux2KleinSettings` field, never reaches
    a settings JSON, and never appears in a log line or an error message — it travels as the
    per-call `hf_token` request field and nowhere else. The badge is TRI-state: "not read yet" is
    distinct from "not stored" and is never rendered as it.
  - **Six access verdicts, three of them with a link, and that IS the feature.** `no_token` and
    `invalid_token` link to the token settings page, `not_accepted` to that repository's own page;
    `ok`, `not_found` and `network_error` offer none. Every row also carries the backend's own
    `message`, shown on hover and logged — the technical half, because `network_error` has no
    localizable content. A literal this build does not know degrades to "not known" with the
    literal on hover and NO link, exactly as the residency block does.
  - **The «Расцензуренный энкодер» toggle is a normal persisted setting**
    (`uncensored_text_encoder`), never a global and never inside `params`: the download methods
    carry it as their own top-level `uncensored` field. It selects which encoder is fetched AND
    repoints `text_encoder_path` — but only when that path is empty or names the OTHER managed
    directory OF THE SAME VARIANT (`config::flux2_klein_text_encoder_dir`), so a hand-picked
    encoder, and the other engine's directory, both survive a flip.
  - **The two `.download.*` methods carry a `variant`** (`"9b"`, the default when absent, or
    `"4b"`), and `check` ECHOES it back — which is why the staleness rule is
    `download_check_matches_selection` over the toggle AND the variant, so an answer about the
    other checkpoint stops being shown instead of pricing the wrong download. Every OTHER method
    is unchanged: they are already keyed by the three component paths, and the prompt-cache
    family is fingerprinted backend-side from the encoder, so 4B gets its own family for free.
  - **Only ONE variant is resident at a time.** The backend keeps one pipeline and one text
    encoder, keyed by the component paths, so selecting the other engine unloads the previous
    model. This side orchestrates nothing and only says so, in one line at the top of
    «Установка модели» (`variant_residency_hint`).
  - **The 4B repository is UNGATED** (apache-2.0), so its download block draws no token row and
    says so in one line instead; the gating hint above it is drawn only for a gated variant.
  - **The download claims the shared progress bar**, so `flux2_pipeline_busy` now spans FOUR
    operations. `run_block_reason` and `start` read `non_run_pipeline_busy` — the same rule with
    the generation taken out, so a run cannot report itself as the reason it cannot start while a
    prompt-cache build, a component action or a download genuinely blocks the next one.
  - **Two progress levels.** `step`/`total` stay the OVERALL counter (bytes across the whole plan),
    which is what keeps every existing single-level consumer correct; `file_step`/`file_total`/
    `file_label` are OPTIONAL and describe the file in flight. A frame without them renders the
    overall bar alone — never a second bar frozen at zero — and a `file_total` of zero renders an
    indeterminate bar rather than dividing.
  - **`plan` is NULLABLE, and an absent plan alone decides that NO size and NO completion
    state may be drawn** — whatever the repository states say. The backend sends `null` for
    all three of "no token", "a repository is inaccessible" and "the file listing failed",
    because a zero-valued plan is the same lie in every one of them: it reads as "nothing
    left to download", and was observed rendering a complete installation on an empty
    machine. Access and listing are two different network operations, so every row can read
    `ok` with no plan behind it; the rows then stay `ok` (authentication really did succeed)
    and `plan_error` carries the reason. `plan_error` is non-empty exactly when the rows
    cannot explain the gap — it is shown on hover beside a localized line, the way a repo
    `message` is — and empty otherwise, when no extra line is synthesised at all. That case
    keeps «Скачать» OPEN, because pressing it re-lists. The four-way decision is the pure
    `flux2_download_readiness`, and `Complete` is reachable only through a plan that was
    actually computed.
  - **Speed and remaining time are DERIVED on this side, with no wire field for either.**
    `Flux2RateEstimator` averages the OVERALL byte counter over a five-second wall-clock
    window — a time window and not a frame count, because the backend's two throttles (~10
    frames/s AND >= 4 MB apart) make the frame cadence depend on the line speed. It is never
    fed the per-file counter, which resets at every file boundary. Nothing is reported until
    the window spans `FLUX2_RATE_MIN_SPAN`, so a transfer shows no estimate at its start
    rather than a wrong one, and a stalled or finished counter reports no rate rather than
    zero or infinity. A BACKWARDS step discards the window and restarts it: a resumed file
    whose length check fails is refetched from zero, so the overall counter really does move
    backwards once per such file (`ipc/PROTOCOL.md`, `download.start`); averaging across that
    seam would report a rate that never happened. The numbers are cleared by
    `begin_progress_generation` and drawn only while the bar is active and the phase is
    `download`, so they never sit frozen beside an idle bar.
  - **A finished download configures the engine**: the answer's three paths are written into the
    settings and marked dirty, only non-empty ones, so a partial answer cannot blank a path the
    user set by hand.
- Every `t!` key of this engine lives under `cleaning.tools.flux2_klein.*` (the download block under
  `cleaning.tools.flux2_klein.download.*`); the run/cancel wording it shares with the older editors
  stays under `cleaning.mask_editor.*`. The `hf_token` module has its own `hf_token.*` namespace,
  because the token is not a cleaning-tool concept.

## Editing map
- To add an engine: a module here plus one line in `mod.rs::all_engines`; the trait it must satisfy
  is `../engine.rs`.
- To change which paths a run uses, or to add a third source mode:
  `Flux2KleinSettings::effective_paths` and `Flux2SourceMode` — never a second computation at a
  call site.
- To change FLUX.2 klein's parameters, prompt block, prompt-cache library, memory presets,
  RAM/VRAM forecast or model-download block: `flux2_klein/`, whose `MODULE_README.md` says which
  file owns which of them. The wire names of its methods live in `backend_ipc::protocol`.
- To change what one of FLUX.2 klein's lines SAYS, edit its pure function and its test, never the
  drawing code (`flux2_klein/decisions.rs`): `flux2_readiness_line` (the readiness line),
  `flux2_prompt_cache_line` (the one line under the prompt), `flux2_component_rows` / `flux2_component_row_tooltip` (a row of the
  merged component list), `estimate_status_line` (the forecast, printed by two surfaces).
- To move a control between FLUX.2 klein's sections: `Flux2PanelCtx::draw` (`flux2_klein/ui/mod.rs`)
  is the whole order, and
  the five bodies under it are `draw_prompt`, `draw_strength`, `draw_readiness`,
  `draw_install_section`, `draw_memory_section` and `draw_advanced_section`.
- To change where the Hugging Face token is stored or to add a SECOND UI surface for it:
  `ms_sysprobe::hf_token`, never here — this engine only reads the global and puts it on the wire.
- To change where the downloaded model lands: `config::flux2_klein_dir` /
  `config::flux2_klein_text_encoder_dir`, together with the Python half's own manifest.
- To change its size contract: `Flux2KleinEngine::frame_constraints` AND `region_block_reason`
  together — the test that compares them will fail otherwise.
- To change which LaMa checkpoints are offered, which backend method one runs, or whether it can
  refine: `lama/catalog.rs` and nothing else — `sdxl/` reads the same table through
  `lama_v2_model_catalog`.
- To change a LaMa parameter, its range or where it is drawn: `lama/engine.rs`
  (`draw_method_parameters`) plus the range constants in `lama/mod.rs`; to change what travels on
  the wire: `lama/wire.rs` (`lama_run_header`), which is also where `effective_refine` lives.
- To change an SDXL parameter, its range or where it is drawn: `sdxl/engine.rs`
  (`draw_mode_parameters`) plus the range constants in `sdxl/mod.rs`; to change what travels on the
  wire: `sdxl/wire.rs` (`sdxl_run_header`), which is also where `lama_model_for_run` lives. To
  change what is persisted, or the on-disk document: `sdxl/settings.rs`.
- To change what the host does with an engine (the picker, the panels, the frame): `../mod.rs` and
  `../../region_edit_v2/`, never here.
