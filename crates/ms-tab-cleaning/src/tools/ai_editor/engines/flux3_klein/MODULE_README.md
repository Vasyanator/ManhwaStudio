# Module: crates/ms-tab-cleaning/src/tools/ai_editor/engines/flux3_klein

## Purpose
The FLUX.3 klein engine of the «ИИ-редактор области» tool: one implementation of
`region_edit_v2::engine::AiEngine` and one picker entry (`flux3_klein`, «С промптом»). It owns
everything model-specific — the persisted parameters, the weight format, the session's extra
reference images, the `inpaint.flux3_klein*` wire contracts, the Hugging Face download of the
gated package, the forecast question, the worker threads and its progress bar — and nothing of
the host: the frame, the painted mask stack and the pending result are the host's.
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

The model-agnostic parts are not here: `../shared/` (progress + `stream_call`, the Hugging Face
check / plan / token / download and their drawing, the settings-file quarantine and save
backoff, `mask_for_run`) and `../../large_model_resources/` (`ResourcesController` + the
two-axis widget with the format rows and «Энкодер на CPU»).

## Files and submodules
- `mod.rs`: root — constants (frame limits mirrored from the backend's `wire.py`, nine extra
  references, steps 1..64 default 16, mask tint, adapter / engine id, repository, default
  prompt, timeouts). Feather and colour-match ranges are `ms_region_postprocess`'s.
- `settings.rs`: `Flux3KleinSettings` (+ `Flux3SourceMode`, `Flux3Format`, `Flux3Fp8Compute`),
  `normalized()`, `effective_root()` (THE package root of both modes), `to_params()` (exactly
  the backend's 13 `FIELDS`), the flattened `postprocess: PostprocessSettings` (persisted,
  never sent), the settings-file load/save.
- `references.rs`: `Flux3References` (session list, capacity, marks-reference room) and
  `read_reference_file` (worker: decode any image, re-encode through the tab's PNG codec).
- `status.rs`: `Flux3Status`, `flux3_status_for_root` (stale guard), `flux3_model_readiness`
  (`Unknown` never blocks; `NoRoot` / `Missing` do).
- `wire.rs`: `flux3_run_request`, `run_flux3_klein` (answer + the run's mask and mode,
  `Flux3RunOutcome`), `parse_flux3_run_answer` (`Flux3RunAnswer`),
  `fetch_flux3_status`, `unload_flux3_klein`.
- `download.rs`: the token + format header (the ONE place the token goes on the wire), the
  echo-format staleness rule, the two download calls over the shared ones.
- `decisions.rs`: readiness line, run gate, seed field (`parse_seed_input`: blank = random),
  stage names (`flux3_stage_label`), the finished-run line (names the seed).
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
- **Post-processing is the host's**: every run answers `EnginePoll::DoneRaw` with the raw
  region, the mask the run sent and `whole_region` (`sample_mask: None`: FLUX.3 has no separate
  latent mask), and `postprocess_settings()` is `Some(settings.postprocess.normalized())` for
  the engine's whole life. The four persisted keys (`mask_feather_px`, `color_match`,
  `color_match_local_pct`, `color_match_scale_px`) are flattened, so an older file keeps its
  values and the saved key set is unchanged (pinned by a test).
- **The seed is the user's or random**: blank field = `null` (the backend draws one and echoes
  it, shown in the run line and offered as «Повторить этот сид»); an invalid text blocks the run
  and is never persisted.
- **References are session state**, at most nine together with the marks reference; with nine
  user references `marks_support()` offers only «На редактируемую картинку».
- **One progress bar, claimed by generation** (a run and a download): a cancel retires the
  generation and cancels the request — sticky, also before the request id exists — and the
  backend stops at its next stage or denoise step. The cancelled worker is waited for
  (`abandoned`): no run or download starts until it ended (`cancel_pending_error`).
- **The forecast asks about the run's exact references**: the region, each user reference, and
  the region-sized marks reference when the host says one would travel
  (`RunOptionsCtx::marks_reference`, learned in `draw_run_options`; `start` corrects it).
- **A success reply is parsed strictly**: every v15-required field (`seed`, `steps`, `width`,
  `height`, `text_tokens`, `applied.*`, the step ids, `degraded`, `degrade_reason`) is checked;
  a malformed one is `malformed_answer_error` naming the field, never a run with an unknown seed.
- **The download writes nothing into the settings**: the download-mode root is derived
  (`config::flux3_klein_dir`, mirrors `flux3_download.model_root`); a different answered root is
  logged as drift. The token is never persisted or logged.
- **A damaged settings file is never overwritten** (shared rules); a JSON array is no document.

## Editing map
- A persisted parameter: `settings.rs`, then the `ui/` file that draws it. Feather / colour
  match: `ms_region_postprocess` (settings, ranges, body UI) and `../shared/postprocess_section.rs`.
- What a line or refusal SAYS: `decisions.rs` + its test; what `.status` MEANS: `status.rs`.
- A request or response shape: `wire.rs` / `download.rs` — a protocol change (bump
  `PROTOCOL_VERSION` on both sides; the Python split is `ipc/handlers/flux3_klein.py`).
- The size rule: `Flux3KleinEngine::frame_constraints` AND the backend's `wire.py` together.
- What happens when a control is pressed: `engine/actions.rs`; what is kept between frames and
  when a query is re-armed: `engine/mod.rs`.
- Texts: `cleaning.tools.flux3_klein.*` in `crates/ms-i18n/locales/{en,ru}.json`; a new key also
  goes into `test_support.rs`'s key list (the shared «Постобработка» keys are `POSTPROCESS_KEYS`).
