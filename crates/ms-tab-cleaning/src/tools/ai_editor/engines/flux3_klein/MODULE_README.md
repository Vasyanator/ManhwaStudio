# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux3_klein

## Purpose
The FLUX.3 klein engine of the «ИИ-редактор области» tool: one implementation of
`region_edit_v2::engine::AiEngine` and one picker entry (`flux3_klein`, «С промптом»). It owns
everything model-specific — the persisted parameters, the DiT and text-encoder weight formats,
the session's extra reference images, the `inpaint.flux3_klein*` wire contracts, the catalog
bundle a selection needs (downloaded by the app's model catalog), the forecast question, the
worker threads and its progress bar — and nothing of the host: the frame, the painted mask stack and the pending result are the host's.
`Flux3KleinEngine` is the only `pub` item.

## Architecture
FLUX.2 klein's layout: `mod.rs` is the root and holds no logic (header, `mod x; use x::*;`
wiring, shared constants); every child starts with `use super::*;`, so any file names any item
without a `super::sibling::` path. Three layers, one-way:

```
ui/            draws            -- reads decisions.rs and the engine's accessors, raises intents
decisions.rs   decides          -- pure, no Ui, no channel, no disk
engine/        owns the state   -- builds the panel context, folds the intents, runs workers
```

One edit:

```
host frame + painted mask (+ marks reference) + session references
  -> AiEngine::start -> run_input (sizes, reference count, mask_for_run)
  -> settings.normalized().to_params(whole_region)       (the ONLY wire params)
  -> run_flux3_klein (worker, streaming, generation-guarded bar, stage ids)
  -> parse_flux3_run_answer (strict image_len, exact size, echoed seed)
  -> AiEngine::poll -> EnginePoll::DoneRaw(raw region, mask, whole_region)
  -> host: ms_region_postprocess with AiEngine::postprocess_settings (re-read every frame)
```

How the model edits a region (backend `inpaint/flux3_klein/region.py`): FLUX.3 has no mask
input, so the REGION is reference #0 and the output is the region's size; the painted mask is
permission to change: the backend only validates it and answers the RAW region (protocol 17), and
the HOST composites it inside the mask (feather + colour match, `ms_region_postprocess`, the
engine's persisted «Постобработка» settings), so everything outside stays byte-identical and a
settings change re-derives a pending result without a new run; an EMPTY mask is the
whole-region mode (`mask_for_run`), colour-matched against the region's edge band instead of a
ring. User references
follow as #1.., the marks reference is appended last.

The download (plan D4, D7, protocol 24): the package is pinned in the app's catalog
(`ms_sysprobe::ai_models::external_catalog::flux3_klein`: base + the root DiT config for a bf16 /
GGUF DiT + DiT(format) + encoder(format); the repository id is the catalog's
`FLUX3_KLEIN_BFL_REPO`,
`DirLayout::Shared` under `config::flux3_klein_dir`). In download mode the engine's
`OcrModelDownloadController` probes the selection's bundle (`flux3_catalog_bundle`), runs the
download / verify job (access preflight of the gated BFL repository, resumable) and adopts an
existing tree by hash; the panel draws it through `draw_ocr_model_download`. The backend
downloads nothing. Every request carries every format CANDIDATE (`dit_formats`, the GGUF
`text_encoder_formats`, absolute paths under the root), so `.status` answers per-format
presence and no installed-formats probe exists.

The model-agnostic parts are not here: `../shared/` (progress + `stream_call`, the token row and
its worker, the catalog-download decisions, the settings-file quarantine and save backoff,
`mask_for_run`) and `../../large_model_resources/` (`ResourcesController` + the two-axis widget
with the DiT format rows and «Энкодер на CPU», and `draw_encoder_formats` for the text
encoder's format rows; D16 option a), and the prompt library — `../shared/prompt_cache.rs`
(controller, gates, cache line, jobs) and `prompt_cache_ui.rs` (cache row, line, library fold),
the FLUX.2 klein UX unchanged. The forecast answer carries the encoder's plan
(`profiles.encoder`, null exactly when the run does not count the encoder) and its format rows
(`encoder_formats`, ids = `Flux3EncoderFormat::wire()`); the widget sums the two handles into the
readiness line's figure and draws the encoder line as its breakdown — nothing FLUX.3-specific.

The prompt library (protocol 24, the protocol-23 shapes of FLUX.2 klein): one library
`flux3_klein` (`ms_config::FLUX3_PROMPT_LIBRARY_ID`) for every encoder format, named presets
keyed by their exact text, entries ranked by `flux3_prompt_cache_precision_order`. Every call
carries `prompt_library` and `prompt_cache_precision_order` in its params; the four
`.prompt_cache.*` calls send `{params, name? | file? path? | path?}` (FLUX.2's header). The
cache serves TEXT encoder mode only (`text_mode_in_effect`): in `vl_refs` the encoder also reads
the images, so «Кэшировать» is closed with that reason and a VL line replaces the cache line.
`.status` answers `prompt_cached` / `prompt_cache_entry` about the prompt that travelled; served
in TEXT mode, it WAIVES the text encoder in `flux3_model_readiness` (the run gate and the
readiness line; the encoder weights may be absent) — the backend then never acquires the
encoder, and its forecast (`profiles.encoder`) leaves the encoder out unless it is resident.

## Files and submodules
- `mod.rs`: root — constants (frame limits mirrored from the backend's `wire.py`, nine extra
  references, steps 1..64 default 16, mask tint, adapter / engine id, repository, default
  prompt, timeouts). Feather and colour-match ranges are `ms_region_postprocess`'s.
- `settings.rs`: `Flux3KleinSettings` (+ `Flux3SourceMode`, `Flux3Fp8Compute`; the formats are
  `ms_config::{Flux3DitFormat, Flux3EncoderFormat}` via `dit_format()` / `encoder_format()`),
  `normalized()` (D6: `vl_refs` off with a GGUF encoder), `text_mode_in_effect()` (the encoder
  mode the wire carries), `effective_root()` (THE package root of both modes), `to_params()`
  (exactly the backend's 18 `FIELDS`, the format candidates and the prompt-library id and
  precision order included), `package_path()`, the flattened `postprocess: PostprocessSettings`
  and the «Нет» stash `uncached_prompt` (both persisted, never sent), the settings-file
  load/save.
- `references.rs`: `Flux3References` (session list, capacity, marks-reference room) and
  `read_reference_file` (worker: decode any image, re-encode through the tab's PNG codec).
- `status.rs`: `Flux3Status` (per-format rows keyed by wire id: `formats` for the DiT,
  `text_encoder_formats` for `bf16` and the GGUF encoders; an encoder reads its own row,
  never the selection-bound `text_encoder_present`; the cache half `prompt_cached` /
  `prompt_cache_entry` with the prompt it was asked about), `flux3_status_for_root` (stale
  guard), `flux3_model_readiness` (`Unknown` never blocks; `NoRoot` / `RootNotAbsolute` /
  `Missing` do; a cached prompt in TEXT mode waives the encoder).
- `wire.rs`: `flux3_run_request`, `run_flux3_klein` (answer + the run's mask and mode,
  `Flux3RunOutcome`), `parse_flux3_run_answer` (`Flux3RunAnswer`),
  `fetch_flux3_status` (records the prompt asked about), `unload_flux3_klein`, and the prompt
  library's `FLUX3_PROMPT_CACHE_METHODS` + `flux3_prompt_cache_header`.
- `decisions.rs`: readiness line (with `BundleMissing`), run gate, seed field
  (`parse_seed_input`: blank = random), stage names (`flux3_stage_label`), the finished-run line
  (names the seed), the catalog bundle (`flux3_catalog_bundle`), the format names, hints and
  picker rows (name, catalog size, ✓ from the guarded `.status`), the prompt library's gate
  context (`flux3_cache_context`: source `stock`, the encoder format in download mode, the FLUX.3
  ranking, the `vl_refs` block) and encoder caption (`flux3_encoder_caption`).
- `engine/`: `Flux3KleinEngine` and its long operations. Own `MODULE_README.md`.
- `ui/`: the panel. Own `MODULE_README.md`.
- `test_support.rs`: `cfg(test)` fixtures and the list of every key the code spells.

## Contracts and invariants
- **The GUI thread never blocks**: settings IO, reference reads, every IPC call and the file
  dialogs run on workers; `poll` only drains channels and runs every frame.
- **The size rule has one owner**: `frame_constraints()` declares it, `check_size` decides it,
  `region_size_refusal` re-checks it on the run path; it equals the backend's
  `validate_region_size` (16 grid, side >= 128, area <= 4 MP, aspect <= 4:1), pinned by a test.
- **`normalized().to_params()` is the only wire value.** An unknown key is refused by the
  backend by name, so the key set is pinned by a test; `fp8_compute` travels as chosen only for
  fp8r; `encoder_on_cpu` (default on, also for an old file) travels in the run, the status and the
  forecast. The «Постобработка» settings never travel (the backend refuses their keys).
- **Every format candidate travels, in both modes** (D7): `dit_formats` is the full DiT order
  (`Flux3DitFormat::all`), natives without a path, GGUF ids with `package_path(root, catalog
  relative path)`; `text_encoder_formats` the GGUF encoders. The ids and the paths are Rust's;
  the backend treats GGUF ids as opaque. Without an ABSOLUTE root (`has_usable_root`: manual,
  nothing picked, or a relative / half-typed folder) no `.status`, forecast or run is asked —
  the paths would not be absolute and the backend would refuse the whole call; the verdict is
  `NoRoot` / `RootNotAbsolute`, decided locally.
- **A GGUF encoder is text-only** (D6): `normalized()` sends `encoder_mode = text`, the stored
  `vl_refs` is kept, and the panel closes the checkbox with the reason on hover and under it.
- **Post-processing is the host's**: every run answers `EnginePoll::DoneRaw` with the raw
  region, the mask the run sent and `whole_region` (`sample_mask: None`: FLUX.3 has no separate
  latent mask), and `postprocess_settings()` is `Some(settings.postprocess.normalized())` for
  the engine's whole life. The four persisted keys (`mask_feather_px`, `color_match`,
  `color_match_local_pct`, `color_match_scale_px`) are flattened, so an older file keeps its
  values and the saved key set is unchanged (pinned by a test).
- **A cached prompt waives the encoder only with `Some(true)` in TEXT mode**: the `.status`
  cache half counts only for the root AND the prompt it was asked about (`prompt_cache_state`);
  `None` waives nothing, `vl_refs` never. The DiT and the AE are never waived. `.status` is
  re-asked when the prompt or the encoder mode changes (`note_settings_changed`).
- **One pipeline**: a prompt-cache operation is part of `pipeline_busy` (it streams on the run's
  bar and reads the encoder): it blocks a run (`prompt_cache_busy_error`), the `.status`, forecast
  and listing questions wait for it, and every library control is closed during a run.
- **The seed is the user's or random**: blank field = `null` (the backend draws one and echoes
  it, shown in the run line and offered as «Повторить этот сид»); an invalid text blocks the run
  and is never persisted.
- **References are session state**, at most nine together with the marks reference; with nine
  user references `marks_support()` offers only «На редактируемую картинку».
- **One progress bar, claimed by generation** (the run): a cancel retires the generation and
  cancels the request — sticky, also before the request id exists — and the backend stops at its
  next stage or denoise step. The cancelled worker is waited for (`abandoned`): no run starts
  until it ended (`cancel_pending_error`).
- **The forecast asks about the run's exact references**: the region, each user reference, and
  the region-sized marks reference when the host says one would travel
  (`RunOptionsCtx::marks_reference`, learned in `draw_run_options`; `start` corrects it).
- **A success reply is parsed strictly**: every v15-required field (`seed`, `steps`, `width`,
  `height`, `text_tokens`, `applied.*`, the step ids, `degraded`, `degrade_reason`) is checked;
  a malformed one is `malformed_answer_error` naming the field, never a run with an unknown seed.
- **The catalog download writes nothing into the settings**: the download-mode root is derived
  (`config::flux3_klein_dir`, which the catalog's `Shared` dirs mirror, pinned by a catalog
  test). It never claims the run's progress bar (it is not part of `pipeline_busy`); a running
  catalog job closes the engine picker (`catalog_switch_block_reason`), and its end re-asks
  `.status`, the forecast and the library listing. A change of the selection re-probes the
  bundle and re-asks `.status` (and the listing when the encoder or the mode moved). The controller is `None` in a test build (its adoption would hash the
  developer's real files). The token is the process-wide `ms_sysprobe::hf_token`, edited by the
  shared row while a gated spec of the bundle is not installed; never persisted or logged here.
- **A damaged settings file is never overwritten** (shared rules); a JSON array is no document.

## Editing map
- A persisted parameter: `settings.rs`, then the `ui/` file that draws it. Feather / colour
  match: `ms_region_postprocess` (settings, ranges, body UI) and `../shared/postprocess_section.rs`.
- What a line or refusal SAYS: `decisions.rs` + its test; what `.status` MEANS: `status.rs`.
- A request or response shape: `wire.rs` (and `to_params` in `settings.rs`) — a protocol
  change (bump `PROTOCOL_VERSION` on both sides; the Python split is
  `ipc/handlers/flux3_klein.py`).
- Which files a format needs, where they lie under the root, a new format: the catalog
  (`external_catalog/flux3_klein.rs`) and the vocabulary (`ms_config::flux3_weight_format`);
  this engine follows both without a per-format table of its own (except the native format
  captions in `decisions.rs`).
- The size rule: `Flux3KleinEngine::frame_constraints` AND the backend's `wire.py` together.
- What happens when a control is pressed: `engine/actions.rs`; what is kept between frames and
  when a query is re-armed: `engine/mod.rs`.
- The prompt library's FLUX.3 facts (methods, header, gate context, caption, the TEXT-only
  rule): `wire.rs`, `decisions.rs`, `settings.rs::text_mode_in_effect`; its gates, line, jobs
  and drawing: `../shared/prompt_cache*.rs` (never copied here).
- Texts: `cleaning.tools.flux3_klein.*` in `crates/ms-i18n/locales/{en,ru}.json`; a new key also
  goes into `test_support.rs`'s key list (the shared «Постобработка» keys are `POSTPROCESS_KEYS`;
  the prompt library's are the shared `cleaning.tools.prompt_library.*`).
