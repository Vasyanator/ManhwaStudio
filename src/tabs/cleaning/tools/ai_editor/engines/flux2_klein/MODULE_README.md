# Module: src/tabs/cleaning/tools/ai_editor/engines/flux2_klein

## Purpose
The FLUX.2 klein engine of the «ИИ-редактор области» tool: one implementation of
`ai_editor::engine::AiEngine`. It owns everything model-specific — the persisted
parameters, the memory presets, the RAM/VRAM forecast, the prompt-cache library, the
model download, the per-component residency, the request/response wire contracts, the
OOM recovery, the worker threads and its own progress bar — and nothing host-specific:
the on-canvas frame, the painted mask stack and the pending result belong to the host.

The module path is `engines::flux2_klein`. `Flux2KleinEngine` is the only `pub` item;
everything else is `pub(super)` at most and is re-exported into the module root, so the
rest of the tree sees one flat `flux2_klein::*` namespace.

TWO ENGINES, ONE IMPLEMENTATION. `config::Flux2Variant` (`Klein9B` / `Klein4B`) is the
parameter, and `all_engines` builds one `Flux2KleinEngine::new(variant)` per value. The
variant keys the engine id, the picker caption, the model directory, the three derived
component paths, the settings FILE, and the `variant` field the two `.download.*` methods
carry. It is fixed at construction and never changes for an instance. The enum is declared
in `config.rs` (runtime path decisions belong there, and it keys five path functions);
`variant.rs` here adds a SECOND inherent `impl` with the model/UI facts, which is legal
within the defining crate and is what keeps a single enum instead of a mapping table.

The 4B checkpoint differs in exactly two user-visible ways, both OMISSIONS: its repository
is ungated (apache-2.0), so the download block draws no Hugging Face token row, and no
uncensored text encoder is published for it, so that toggle is not drawn — and
`normalized()` forces the flag off, because the backend refuses `variant = "4b"` with
`uncensored = true`. Both omissions have a recovery: the staleness rule compares the
EFFECTIVE toggle (`uncensored_encoder_active`), so a document carrying the inert flag still
accepts its own check answer, and a check that nevertheless blames the token brings the
token row back with a line explaining why — `hf_token` has no other UI surface, so hiding
it unconditionally would strand the user.

Everything else, on screen and on the wire, is identical: every other method is keyed by
the three component paths, and the prompt-cache family is fingerprinted backend-side from
the encoder, so 4B gets its own family without a wire change.

Only ONE variant is resident at a time — the backend holds one pipeline and one text
encoder, keyed by those paths — so selecting the other engine unloads this one. This side
orchestrates nothing and only says so, in one line at the top of «Установка модели».

## Architecture
`mod.rs` is the root and holds NO logic: the module header, the module declarations and
the constants everything below shares (the selection limits, the parameter ranges, the
mask tint, `FLUX2_DEFAULT_PROMPT` and the three status colours). Every other item lives
in a submodule.

The root declares each child as `mod x; use x::*;`, and each child starts with
`use super::*;`. A private `use` is visible to a module's descendants, so the glob chain
carries the root's imports and every sibling's items down through `ui/` and `engine/`
too: a file here may name any item of this module regardless of which file holds it, and
never through a `super::sibling::` path. The one exception to the glob is
`Flux2KleinEngine`, which is `pub` and is re-exported explicitly — a private glob would
not carry it out of the module.

Three layers, and the direction between them is one-way:

```
ui/            draws            -- calls decisions.rs and the engine's accessors
decisions.rs   decides          -- pure, no Ui, no channel, no disk
engine/        owns the state   -- builds the panel context, runs the workers
```

Data flow of one edit:

```
host frame + painted mask
        -> AiEngine::start           (engine/mod.rs)
        -> mask_for_run              (session.rs)   derives the working mode
        -> settings.normalized()     (settings.rs)  the ONLY value put on the wire
        -> run_flux2_klein           (wire.rs)      worker thread, streaming call
        -> publish_progress_frame    (progress.rs)  generation-guarded bar writes
        -> AiEngine::poll            (engine/mod.rs) drains the channel, returns pixels
```

Queries the panel arms are one-shot and independent of the run: `.status`
(`status.rs` + `wire.rs`), `.estimate` (`estimate.rs`), `.prompt_cache.list`
(`prompt_cache.rs`) and `.download.check` (`download.rs`).

## Files and submodules
- `mod.rs`: the module root — the header, the `mod`/`use` wiring and the shared constants.
  Edit it to add a submodule or a constant several files share, and nothing else.
- `variant.rs`: the model/UI half of `config::Flux2Variant` — `engine_id`, `title`,
  `model_repo`, `supports_uncensored_encoder`, `requires_hf_token`. It declares no items of
  its own, only a second inherent `impl`, so it is a bare `mod` with no glob. Edit it to add
  a fact that differs between the checkpoints and is NOT a path; a path belongs in
  `config.rs` beside the enum.
- `engine/`: the engine. `mod.rs` holds `Flux2KleinEngine`, its `AiEngine` impl and the
  polling that keeps its derived state current; `actions.rs` holds the `start_*`/`poll_*`
  pair of every long operation. See `engine/MODULE_README.md`.
- `ui/`: the parameter panel. `mod.rs` is the body and its ORDER; `install.rs`,
  `components.rs`, `advanced.rs` and `progress.rs` draw the blocks it calls into. See
  `ui/MODULE_README.md`.
- `decisions.rs`: the PURE decisions the panel renders — `flux2_readiness_line`,
  `flux2_prompt_cache_line`, `flux2_guidance_supported`, `flux2_run_block_reason`,
  `region_block_reason` — with the line/tone types they answer in. Edit it to change what a
  line or a refusal SAYS, or which control the backend's answer closes.
- `settings.rs`: `Flux2KleinSettings`, its defaults, `normalized()`, `params()`, the
  placement/dtype/preset vocabulary, `Flux2SourceMode`, the persisted `variant` and
  `effective_paths`, and the settings-file load/save pair. Edit it to add or change a
  persisted field. ONE FILE PER VARIANT: 9B keeps `flux2_klein_settings.json` unchanged,
  the loader stamps the owning variant over whatever it read, and the saver writes back to
  the file the document itself names.
- `status.rs`: `Flux2Status` and everything derived from it — `Flux2ModelReadiness`,
  `flux2_status_for_paths`, the merged `flux2_component_rows`, `flux2_install_seed`,
  `flux2_pipeline_busy`. Edit it to change what a catalog MEANS; the parsers that build
  one live in `wire.rs`.
- `download.rs`: the `.download.check` / `.download.start` contracts, the per-repository
  state vocabulary and its links, `flux2_download_readiness`,
  `download_check_matches_selection` (stale over the encoder toggle AND the variant),
  `download_check_variant_mismatch` (a disagreeing echo is REPORTED, not hidden),
  `download_check_blames_token` (what reopens the token row on the ungated checkpoint) and
  `flux2_text_encoder_path_after_toggle`.
- `prompt_cache.rs`: the `.msprompt` library — entry/listing/load shapes,
  `flux2_prompt_cache_gates`, `prompt_cache_state_for`, and the six `.prompt_cache.*`
  calls with their parsers.
- `estimate.rs`: `Flux2Estimate`, the `peak_*` breakdown keys, the fetch/parse pair and
  the two strings the forecast is rendered as.
- `progress.rs`: `Flux2Progress` and its `Mutex` discipline, the generation counter,
  `Flux2RateEstimator`, the streamed frame parsers and the byte/rate/duration formatters.
- `session.rs`: `Flux2SessionState` (run channel + per-RUN undo stack), `Flux2RunPoll`,
  `mask_for_run`, `apply_backend_flags` and `spawn_flux2_picker`.
- `wire.rs`: `run_flux2_klein` with its OOM retry pass, `flux2_stream_call`, the
  `.status` and `.component_action` calls and their parsers, the prompt translation and
  the image/mask PNG encoding.
- `test_support.rs`: `cfg(test)` only. The fixtures every `mod tests` here is built from
  (`runnable_settings`, `cacheable_settings`, `status_with_present`,
  `status_with_components`, `FLUX2_ALL_COMPONENTS`, `region_rect`,
  `engine_with_settled_region`). A fixture lives here as soon as a second file needs it;
  it is never copied.

## Contracts and invariants
- **The GUI thread never blocks.** Every function in `wire.rs`, `download.rs`,
  `prompt_cache.rs`, `engine/actions.rs` and the IO in `settings.rs` and `session.rs`
  runs on an `ms_thread::spawn` worker; `AiEngine::poll` only drains channels.
- **`normalized()` is the only value that reaches the wire** (`settings.rs`), and
  `effective_paths` is the ONE answer to which three model paths a call uses. Both are
  variant-aware: the derived paths sit under the variant's own model directory, and a
  refused `variant`/`uncensored` pairing is normalized away before it can travel.
- **`mask_for_run` is the only place the working mode is decided** (`session.rs`): a
  painted mask travels verbatim with `whole_region = false`; an empty one becomes a
  SOLID mask, because the backend refuses `whole_region = true` otherwise.
- **One progress bar, claimed by generation** (`progress.rs`): four operations — a run,
  `.prompt_cache.build`, `.component_action`, `.download.start` — share it, and a write
  from a retired generation is dropped. Gates that mean "wait for the current operation"
  read `flux2_pipeline_busy` over all four.
- **`Flux2ModelReadiness` is three-state on purpose** (`status.rs`): `Unknown` never
  blocks a run, and a catalog counts only while it still describes the configured paths.
- **An ABSENT `guidance_supported` means SUPPORTED** (`status.rs`, `wire.rs`,
  `decisions.rs`). `.status` reports whether the loaded checkpoint can use guidance at all:
  a checkpoint declaring `"is_distilled": true` makes diffusers switch classifier-free
  guidance off, so a `guidance_scale` above 1.0 only doubles the per-step compute. The
  field is `Option<bool>` like `prompt_cached` and `text_encoder_available`, but its safe
  default is INVERTED — only a positive `false` closes the control, because an older
  backend reports nothing and must not lose a working parameter. The one place that rule
  lives is `flux2_guidance_supported`. The stored `guidance_scale` is never clamped,
  rewritten or hidden by this: the backend owns the run's semantics and the setting has to
  come back on a checkpoint that is not distilled.
- **`FLUX2_MAX_SEQ` (wire.rs) is part of the prompt-cache key.** Lowering it invalidates
  every saved `.msprompt` entry at once.
- **The HF token never enters the settings file** and must never be logged
  (`download.rs`).
- Layering: only `ui/` may draw, and it decides nothing a test would want to assert —
  the verdicts come from `decisions.rs` and the state belongs to `engine/`. No file here
  reaches another through a `super::sibling::` path, so none depends on another's private
  detail.
- Visibility: nothing in this directory is `pub` except `Flux2KleinEngine`. `pub(super)`
  is the ceiling for everything else, which inside `ui/` and `engine/` already confines an
  item to its own directory.

## Editing map
- To add or change a persisted parameter, see `settings.rs`, then the `ui/` file that
  draws it.
- To add a CHECKPOINT, see `config::Flux2Variant` (identity and paths) and `variant.rs`
  (model/UI facts); `all_engines` then picks it up without a new module.
- To change what differs between the checkpoints on screen, see `ui/install.rs` — both
  differences are `if variant.…()` omissions there and nowhere else.
- To change what the readiness line, the prompt-cache line or a run refusal SAYS, see
  `decisions.rs`; to change what a `.status` answer MEANS, see `status.rs`; to change how
  either LOOKS, see `ui/`.
- To change a request or response shape, see `wire.rs`, `download.rs` or
  `prompt_cache.rs` — the file that owns that method family.
- To change the progress bar's arithmetic, see `progress.rs`; to change how it looks, see
  `ui/progress.rs`.
- To change the run's undo history or the derived mask, see `session.rs`.
- To change what the engine DOES when a control is pressed, see `engine/actions.rs`; to
  change what it keeps between frames, see `engine/mod.rs`.
- Tests live beside the code they exercise, in a `#[cfg(test)] mod tests` at the end of
  each file, and their shared fixtures live in `test_support.rs`. A test that genuinely
  spans two files stays in `mod.rs`.
