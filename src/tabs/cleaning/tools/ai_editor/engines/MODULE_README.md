# Module: src/tabs/cleaning/tools/ai_editor/engines

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

## Files and submodules
- `mod.rs`: the catalog — `all_engines()` and nothing else.
- `flux2_klein.rs`: the FLUX.2 klein engine (IPC methods `inpaint.flux2_klein` streaming,
  `.status`, `.estimate`, `.unload`, and the six `.prompt_cache.*` methods). The user paints the
  area the model is ALLOWED to change, writes a prompt and gets that area regenerated; everything
  outside the painted mask must survive untouched, which is why the engine declares exactly ONE
  mask layer and puts its bytes on the wire verbatim. Leaving that layer EMPTY is the other
  working mode — the whole region is regenerated — and it is derived, not chosen; see the
  contracts below. Size contract:
  multiple of 16, shortest side >= 128 px, area <= 1 MP, aspect not steeper than 8:1, declared
  through `constraints()` AND re-checked against the actual region size on the run path. The prompt
  field is doubled: an optional user-language field plus a Google/Yandex/DeepL picker and a «↓»
  button fill in the ENGLISH field that is the only one sent, reusing the translation tab's own
  `translate_texts_via_translator` on a worker thread. That English field is never empty:
  `FLUX2_DEFAULT_PROMPT` is substituted both for a new settings file and for one whose prompt is
  missing or blank, because an empty prompt blocks the run gate. Under it sits the PROMPT CACHE
  block: `.status` answers `prompt_cached` for the prompt it was asked about (an optional field —
  an absent one reads as "not known", never as "not cached"), and the line above the buttons is
  green/amber/neutral accordingly, shown only while that answer still describes the prompt in the
  field. «Кэшировать» runs the streaming `.prompt_cache.build` on the SAME progress bar as a
  generation (so neither can start while the other runs — reading the ~16 GB Qwen3 encoder takes
  ~106 s, against ~6 s for a cached prompt). The saved caches form a LIBRARY that lives
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
  VAE) are entered by hand or through a non-blocking native picker and persist to
  `flux2_klein_settings.json` (`config::flux2_klein_settings_path`). Memory placement is chosen
  through four BUILT-IN presets («Максимум скорости» / «Сбалансированный» / «Минимум RAM» /
  «Минимум VRAM»); «Пользовательский» is never selectable — it is what the picker reports when the
  SEVEN fields a preset owns match none of them (placement, `low_cpu_mem_usage`, VAE
  tiling/slicing, `unload_transformer_before_vae`, `unload_text_encoder_after_encode`,
  `text_encoder_fp8`). The last two are the text-encoder memory controls: the Qwen3 encoder is
  ~16 GB and is needed exactly once per generation, so every economical preset drops it right after
  the prompt is encoded, while `text_encoder_fp8` is `false` in EVERY preset — quantizing costs
  embedding quality and is the user's decision alone. There is no negative prompt and there must
  not be one: the checkpoint is distilled (4 steps, guidance 1.0). The RAM/VRAM forecast is
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
  FLUX.2 klein's debounced saver runs inside `poll`; a host that stops polling keeps the tool
  working and quietly discards every model path, memory preset and prompt on exit. The arming rule
  is the pure `settings_save_due(dirty, settings_loaded, save_in_flight)`: a write before the
  initial load lands would overwrite the user's file with the in-memory defaults, so `dirty` is
  kept pending instead of dropped, and at most one writer touches the path at a time.
- **The RAM/VRAM forecast is armed by a SETTLED SIZE change and by nothing else geometric.**
  `set_region` is pushed every frame and carries `geometry_settled`; the rectangle is stored
  unconditionally, so the status line keeps printing the size the user is dragging, but the
  `.estimate` round trip is armed only when the region's SIZE changed AND no frame gesture is in
  flight. A pure position change costs nothing — scrolling moves the rectangle through the frame's
  keep-in-view clamp — and a resize drag costs exactly one request, on release, rather than one per
  rendered frame. The size change is held in `region_resize_pending` because the rectangle usually
  no longer changes on the release frame itself. Everything that is NOT the geometry (a parameter,
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
  nothing is painted; the slider is no longer faded for that (the engine cannot see the host's mask
  stack, so the state is unknowable while the panel is drawn) and its hover text names the condition
  instead. `mask_feather_px` keeps working either way and softens the join between the region and the
  page.
- A settings file written before the mode was derived carries a `whole_region` key that maps to no
  field. `Flux2KleinSettings` does NOT use `deny_unknown_fields`, so serde drops it and the document
  loads with every other setting intact; no migration exists for it and none may be added. A test
  pins that such a document loads identically to the same document without the key.
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
  an amber warning line beside the cache status (a warning, not an error — ready caches still work),
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
  `flux2_run_block_reason` answers only for the model paths and the prompt. A test pins that
  `constraints()` and `region_block_reason` accept exactly the same regions.
- Every `t!` key of this engine lives under `cleaning.tools.flux2_klein.*`; the run/cancel wording
  it shares with the older editors stays under `cleaning.mask_editor.*`.

## Editing map
- To add an engine: a module here plus one line in `mod.rs::all_engines`; the trait it must satisfy
  is `../engine.rs`.
- To change FLUX.2 klein's parameters, prompt block, prompt-cache library, memory presets or
  RAM/VRAM forecast: `flux2_klein.rs`. The wire names of its methods live in
  `backend_ipc::protocol`.
- To change its size contract: `Flux2KleinEngine::frame_constraints` AND `region_block_reason`
  together — the test that compares them will fail otherwise.
- To change what the host does with an engine (the picker, the panels, the frame): `../mod.rs` and
  `../../region_edit_v2/`, never here.
