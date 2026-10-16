# Module: tools/large_model_calibration

## Purpose
The measurement and calibration tool of the large-model manager
(`modules/ai_backend/runtime/large_models/`). It runs the REAL model services (FLUX.3 klein,
FLUX.2 klein) on a real GPU under the real manager, records every manager step (device memory,
host memory, wall time, where each component sat), and fits the manager's cost table
`runtime/large_models/calibration/<device_class>.json` from those records. It also judges a run's
forecasts against what was measured (the acceptance numbers). Not part of the shipped runtime:
nothing imports it.

## Running
From the repository root (the directory is run; on a measurement host without `venv/`, run it with
that host's interpreter the same way):

```
venv/bin/python tools/large_model_calibration flux3 --model-root <flux.3-klein root> [...]
venv/bin/python tools/large_model_calibration flux2 --transformer-path <ckpt> --vae-path <dir> \
  --text-encoder-path <dir> --image <photo> [...]
venv/bin/python tools/large_model_calibration fit <report dirs...> --base <calibration json> --out <json>
venv/bin/python tools/large_model_calibration check <report dirs...>
```

`--dry-run` lists a driver's combos without importing torch. A driver writes into a fresh
`lmc_flux3_*` / `lmc_flux2_*` in the system temp dir (or `--out`; a path inside the repository is
refused): `report.json`, `report.csv` (combo x step), `run.log`, `images/`. Run on an otherwise
idle GPU: the device counters are device-wide.

Recalibrating a device class: measure resident cells over several request sizes (the `x` of every
fit) plus the cells that move bytes (park / restore, a host-juggled DiT, a CPU encoder), then
`fit` with the class's current table as `--base` (or the reference table for a new class) and
`--out` the tracked file, then re-run a validation grid and `check` it.

Does FLUX.3 nvfp4 single-reference editing fit in 8 GB, and under which steps?

```
venv/bin/python tools/large_model_calibration flux3 --model-root <root> --variants nvfp4 \
  --sizes 512x512,768x768 --cases edit_kitchen_sage --ref-max-pixels auto \
  --vram-steps max,phase,half,min --ram-steps keep,run,min --vram-cap-gib 8
```

`--vram-cap-gib N` makes the manager treat the card as N GiB (`Limits.vram_cap`, context
included) and caps torch's allocator there as a hard backstop; the context is outside torch's cap
and is reported (`settings.vram_cap`, per step `non_torch_end`).

## Architecture
- `__main__.py`: bootstrap (re-exec into `<repo>/venv/bin/python` when it exists, protobuf env,
  `sys.path`: repo root first, `tools/` last).
- `cli.py`: the subcommands `flux3`, `flux2`, `fit`, `check`.
- `common.py`: option parsing, the output-directory rule, logging, device resolution, the
  residency stack a measured service runs under (`build_large_models`: arbiter + small pool +
  `LargeModelManager`, limits and floors in memory only), `open_device` (baseline before any device
  call, ROCm setup, `--vram-cap-gib`, environment).
- `recorder.py`: `StepRecorder`, the step observer both services accept: per step the device
  peaks (reset at each begin), `mem_get_info`, the in-step host peak (a sampler thread), and the
  manager's placements (`status()` components) at begin and end.
- `measure.py`: `WorkCapture` (wraps the manager's `register` / `acquire` to learn each phase's
  spec `PhaseWork` and cost-table key), the FLUX.3 `Combo`, `forecast_combo` / `selected_answer`
  (the manager's full forecast of the requested cell, before the admission), `run_combo`.
- `flux3.py`: the FLUX.3 matrix (variants x sizes x cases x encoder modes x AE dtypes x the step
  grid) and the optional DiT parity check (`parity.py`); `cases.py` holds the named cases over
  the user's images in `<model-root>/refs/{edit,obj}`.
- `flux2.py`: the FLUX.2 driver (region sizes x encoders x prompt lengths x references x steps)
  through `Flux2KleinInpaintService.inpaint_image_bytes` and its `set_phase_observer` hook; the
  cell forecast is the service's own estimate (`memory.forecast_memory`).
- `report.py`: `report.json` / `report.csv` (rewritten after every combo), the step grids and the
  per-axis monotonic check, the summary.
- `device_memory.py`: the `--vram-cap-gib` cap and the device baseline (amdgpu sysfs, or
  `nvidia-smi`) -> context size and per-step non-torch memory.
- `fit.py`: `fit` (points -> entries -> table, rules in its header) and `check` (forecast vs
  measured per cell, misses, mean over-forecast, monotonic violations).

## Contracts and invariants
- The drivers call the services' public entry points (`generate`, `inpaint_image_bytes`); no copy
  of a pipeline lives here. FLUX.3 `forecast_combo` reaches the service's private `_plan` (the
  admission's own request shaping); keep it in step with `Flux3KleinService.generate`.
- Measurement output never goes into the repository; the tool never writes the backend's persisted
  limits or learned floors (`MemoryLimitsStore`); the floors are forgotten before every combo unless
  `--keep-learned-floors`, so each combo measures the steps it asks for.
- Cold start per combo (the service's `unload` + allocator cache release); files stay in the OS
  page cache, so only the first read of a file is a cold disk read.
- A step that begins while another is open is recorded with `overlapped` and is not used by a fit.
- `fit` writes `measured: true` only for entries fitted from real points; the catch-all entries and
  every entry the runs did not reach come from `--base` (their `measured` kept only when the base
  is of the same device class). The written table passes the manager's own `parse_tables`.
- The fit covers every measured point: allocated activations by the upper envelope, reserved bytes
  through `reserved_factor`. A component load's host transient is not a table fact: the forecast
  derives it from the inventory (`ComponentInventory.load_host_bytes`: the largest file tensor's
  read buffer plus its non-aliasing conversion outputs), so a report's `forecast_host_peak_gib`
  of a load step's cell already contains it.
- `check` judges host growth of a report's first combo that ran apart: it also holds the process's
  one-time initialisation, which no plan contains.

## Known gaps
- FLUX.2's bf16 and fp8 text encoders share one cost key (the format is the pipeline's `bf16`):
  the fitted entry is the envelope of both (fp8's dequantisation temporaries are larger).
- A device class on which no phase of a family runs wholly resident (FLUX.2 9B on a 12 GB card)
  gets that family's byte entries from juggled phases (staging taken out) but no seconds: `fit`
  keeps the base's seconds there (named in the table's `source`).
- Cold disk rate: a cold read needs a dropped page cache (root); `fit` keeps `--base`'s
  `disk_cold_bytes_per_s`, also used by the forecast for a DISK-juggled pass.
- The Windows NVIDIA sysmem-fallback heuristic stays unverified until a Windows NVIDIA run.

## Editing map
- New FLUX.3 case or caption: `cases.py`. New encoder mode: `measure.ENCODER_SETTINGS`.
- New measured quantity: `recorder.StepRecorder` + `recorder.record_dict` + `report.CSV_COLUMNS`.
- New model family: a driver module beside `flux3.py` / `flux2.py` that installs a `WorkCapture`
  and a `StepRecorder` and fills `ComboOutcome`s; register it in `cli.py`.
- How entries or device constants are fitted: `fit.py` (its header states the rules).
