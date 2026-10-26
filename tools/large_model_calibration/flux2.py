"""
File: tools/large_model_calibration/flux2.py

Purpose:
The `flux2` subcommand: FLUX.2 klein region edits through the REAL
`Flux2KleinInpaintService.inpaint_image_bytes` under the real large-model
manager, measured step by step through the service's calibration hook
(`set_phase_observer`), over region sizes x encoder (bf16 / fp8) x prompt
length x the «Видеопамять» x «Оперативная память» step grid.

Key declarations:
- `Flux2Combo` - one measured point (the report's combo protocol:
  `combo_id`, `grid_key`, `vram_step`, `ram_step`, `describe()`)
- `add_parser()`, `run()` - the subcommand
- `forecast_flux2()` - the manager's full forecast of the requested cell,
  through the service's own estimate arithmetic (`memory.forecast_memory`:
  pipeline planned first, the encoder on what it leaves)

Notes:
- Cold start per combo (`service.unload()` + allocator cache release); the
  prompt gets a per-combo suffix, so the in-memory prompt cache never skips
  the encode.
- The region is a center crop of `--image` resized to cover the size; the
  mask is the centered half-side square (dilation and feathering are the
  service's defaults).
- `--max-seq` varies the encoder's padded length (the only `x` of the encode
  phase); `--with-reference` passes the region itself as the marks reference
  (its tokens join the denoise sequence).
- The transformer's weight format is the one the service derives from the
  file (`params.derived_format`: `bf16` for safetensors / a folder, `gguf` for
  a GGUF file); the forecast and the run plan exactly that format
  (`large_model.weight_format_of`), and the measured phases are keyed with its
  compute path (`native` / `gguf`) by the `WorkCapture`.
- torch is imported inside functions (a dry run imports none).
"""

from __future__ import annotations

import argparse
import io
import logging
import time
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from modules.ai_backend.runtime.large_models.tiers import RamStep, Steps, VramStep

from .common import parse_list, parse_sizes, resolve_out, setup_logging
from .measure import ComboOutcome, WorkCapture, image_stats, reset_device, selected_answer
from .recorder import GIB, StepRecorder, rss_private

log = logging.getLogger(__name__)

#: Output directory prefix in the system temp dir.
OUT_PREFIX = "lmc_flux2_"

#: Default prompt (an edit a user would type).
DEFAULT_PROMPT = "replace the text with a red apple"

#: `--encoders` values -> `text_encoder_fp8`.
ENCODERS = {"bf16": False, "fp8": True}


@dataclass(frozen=True)
class Flux2Combo:
    """One FLUX.2 point: region `size`, encoder weights (`encoder_fp8`), compute `dtype`,
    prompt length `max_seq`, whether the marks reference is passed, the step pair, and the
    transformer's weight format (`params.derived_format` of `--transformer-path`)."""

    size: tuple[int, int]
    encoder_fp8: bool
    dtype: str
    max_seq: int
    with_reference: bool
    vram_step: VramStep
    ram_step: RamStep
    transformer_format: str = "bf16"

    @property
    def steps(self) -> Steps:
        """The requested step pair."""
        return Steps(self.vram_step, self.ram_step)

    @property
    def size_label(self) -> str:
        """`WxH`."""
        return f"{self.size[0]}x{self.size[1]}"

    @property
    def grid_key(self) -> str:
        """Identifier minus the two steps (the cells of one grid share it)."""
        encoder = "fp8" if self.encoder_fp8 else "bf16"
        # A bf16 transformer keeps the ids of earlier reports; any other format is named.
        weights = "" if self.transformer_format == "bf16" else f"__fmt-{self.transformer_format}"
        return f"flux2__{self.dtype}{weights}__{self.size_label}__enc-{encoder}__seq-{self.max_seq}{'__ref' if self.with_reference else ''}"

    @property
    def combo_id(self) -> str:
        """ASCII file-name-safe identifier."""
        return f"{self.grid_key}__vram-{self.vram_step.value}__ram-{self.ram_step.value}"

    def describe(self) -> dict[str, Any]:
        """The report's descriptive fields of this combo."""
        return {
            "family": "flux2_klein",
            "variant": self.transformer_format,
            "size": self.size_label,
            "case": "region_edit_ref" if self.with_reference else "region_edit",
            "encoder": "fp8" if self.encoder_fp8 else "bf16",
            "ae_dtype": self.dtype,
            "max_sequence_length": self.max_seq,
            "with_reference": self.with_reference,
            "vram_step": self.vram_step.value,
            "ram_step": self.ram_step.value,
        }


def add_parser(subparsers: argparse._SubParsersAction) -> argparse.ArgumentParser:
    """Register the `flux2` subcommand (help text in English)."""
    parser = subparsers.add_parser(
        "flux2",
        help="measure FLUX.2 klein region edits per step through the real service",
        description="FLUX.2 klein measurement through the real Flux2KleinInpaintService, per manager step, over sizes x encoders x prompt lengths x the VRAM x RAM step grid.",
    )
    parser.add_argument("--transformer-path", type=Path, required=True, help="transformer checkpoint (file or diffusers folder)")
    parser.add_argument("--vae-path", type=Path, required=True, help="VAE folder")
    parser.add_argument("--text-encoder-path", type=Path, required=True, help="Qwen3 text encoder folder")
    parser.add_argument("--image", type=Path, required=True, help="source image the regions are cropped from")
    parser.add_argument("--out", type=Path, default=None, help=f"output dir (default: new {OUT_PREFIX}* in the system temp dir)")
    parser.add_argument("--sizes", default="512x512,768x768", help="comma list of WxH regions")
    parser.add_argument("--encoders", default="bf16", help=f"comma list of {list(ENCODERS)} (text_encoder_fp8)")
    parser.add_argument("--dtype", default="bfloat16", help="pipeline compute dtype (bfloat16 / float16)")
    parser.add_argument("--max-seq", default="512", help="comma list of max_sequence_length values (64..512)")
    parser.add_argument("--with-reference", default="no", help="comma list of no / yes: pass the region as the marks reference")
    parser.add_argument("--vram-steps", default=VramStep.AUTO.value, help=f"comma list of {[step.value for step in VramStep]}")
    parser.add_argument("--ram-steps", default=RamStep.AUTO.value, help=f"comma list of {[step.value for step in RamStep]}")
    parser.add_argument("--cells", default=None, help="explicit comma list of vram:ram pairs instead of the --vram-steps x --ram-steps grid")
    parser.add_argument("--steps", type=int, default=4, help="denoise steps")
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--prompt", default=DEFAULT_PROMPT)
    parser.add_argument("--vram-cap-gib", type=float, default=None, help="the manager's vram_cap plus torch's allocator cap (see flux3 --help)")
    parser.add_argument("--device", default="discrete", help="'discrete' (never the iGPU), 'service' (General.ai_device), or cuda:N")
    parser.add_argument("--encoder-on-cpu", action="store_true", help="«Энкодер на CPU»: pin the text encoder to the CPU (off: the steps place it, the measured grid)")
    parser.add_argument("--keep-learned-floors", action="store_true", help="keep the manager's learned step floors between combos")
    parser.add_argument("--dry-run", action="store_true", help="print the combos and exit (no torch import)")
    return parser


def _cells(args: argparse.Namespace) -> list[Steps]:
    """The step pairs to run: `--cells`, else the `--ram-steps` x `--vram-steps` grid (RAM outer)."""
    vram_of = {step.value: step for step in VramStep}
    ram_of = {step.value: step for step in RamStep}
    if args.cells:
        pairs = []
        for token in (token.strip() for token in args.cells.split(",") if token.strip()):
            vram, sep, ram = token.partition(":")
            if not sep or vram not in vram_of or ram not in ram_of:
                raise ValueError(f"--cells: {token!r} is not <vram>:<ram>")
            pairs.append(Steps(vram_of[vram], ram_of[ram]))
        return pairs
    vram_steps = parse_list(args.vram_steps, vram_of, "--vram-steps")
    ram_steps = parse_list(args.ram_steps, ram_of, "--ram-steps")
    return [Steps(vram, ram) for ram in ram_steps for vram in vram_steps]


def _parse_ints(value: str, what: str) -> list[int]:
    """Comma list of positive integers; `ValueError` otherwise (the service clamps the range)."""
    tokens = [token.strip() for token in value.split(",") if token.strip()]
    if not tokens or not all(token.isdigit() and int(token) > 0 for token in tokens):
        raise ValueError(f"{what}: {value!r} is not a comma list of positive integers")
    return [int(token) for token in tokens]


def build_combos(args: argparse.Namespace) -> list[Flux2Combo]:
    """Every combo, size-major, the step cells innermost; `ValueError` on bad options (or an unreadable transformer file)."""
    from modules.ai_backend.inpaint.flux2_klein.params import derived_format

    transformer_format = derived_format(str(args.transformer_path))
    sizes = [size for size in parse_sizes(args.sizes, allow_auto=False) if size is not None]
    encoders = parse_list(args.encoders, ENCODERS, "--encoders")
    seqs = _parse_ints(args.max_seq, "--max-seq")
    references = parse_list(args.with_reference, {"no": False, "yes": True}, "--with-reference")
    cells = _cells(args)
    return [Flux2Combo(size, fp8, args.dtype, seq, reference, cell.vram, cell.ram, transformer_format) for size in sizes for fp8 in encoders for seq in seqs for reference in references for cell in cells]


def _params(args: argparse.Namespace, combo: Flux2Combo, index: int) -> dict[str, Any]:
    """The `inpaint.flux2_klein` params of one combo (prompt made unique per combo: no prompt-cache hit)."""
    return {
        "transformer_path": str(args.transformer_path),
        "vae_path": str(args.vae_path),
        "text_encoder_path": str(args.text_encoder_path),
        "prompt": f"{args.prompt} (calibration run {index})",
        "steps": args.steps,
        "seed": args.seed,
        "dtype": combo.dtype,
        "text_encoder_fp8": combo.encoder_fp8,
        # Explicit, because the wire default is «Энкодер на CPU» on.
        "encoder_on_cpu": bool(args.encoder_on_cpu),
        "max_sequence_length": combo.max_seq,
        "vram_step": combo.vram_step.value,
        "ram_step": combo.ram_step.value,
    }


def _region(image_path: Path, size: tuple[int, int]) -> tuple[bytes, bytes, Any]:
    """(region PNG, mask PNG, region RGB array): `image_path` scaled to cover `size`, center-cropped; mask = centered half-side square."""
    import numpy as np
    from PIL import Image

    width, height = size
    with Image.open(image_path) as opened:
        source = opened.convert("RGB")
    scale = max(width / source.width, height / source.height)
    resized = source.resize((max(width, round(source.width * scale)), max(height, round(source.height * scale))))
    left, top = (resized.width - width) // 2, (resized.height - height) // 2
    region = resized.crop((left, top, left + width, top + height))
    mask = np.zeros((height, width), dtype=np.uint8)
    mask[height // 4 : height - height // 4, width // 4 : width - width // 4] = 255

    def png(image: Image.Image) -> bytes:
        buffer = io.BytesIO()
        image.save(buffer, format="PNG")
        return buffer.getvalue()

    return png(region), png(Image.fromarray(mask, "L")), np.asarray(region)


def forecast_flux2(service: Any, large_models: Any, params: dict[str, Any], combo: Flux2Combo, device: str, rss_before: int | None) -> dict[str, Any]:
    """The requested cell's answer before the admission: the pipeline's `profiles(...).selected`
    (fits, resolved steps, same-as) with the run totals of `memory.forecast_memory` (full forecast of
    both handles, budgets from the live probe); keys as `measure.forecast_combo` (`host_growth` is the
    estimate's host bytes, which carry no baseline)."""
    from modules.ai_backend.inpaint.flux2_klein import hardware, large_model, memory
    from modules.ai_backend.inpaint.flux2_klein.params import normalize_flux2_klein_params

    try:
        normalized = normalize_flux2_klein_params(params)
        options, _device = service.large_model_inputs(params)
        shape = large_model.request_shape(combo.size[0], combo.size[1], with_reference=combo.with_reference, max_sequence_length=options.max_sequence_length, steps=options.steps)
        report = large_models.profiles(service.large_model_name(params), options, shape, device=device, format=large_model.weight_format_of(options), steps=combo.steps)
        answer = selected_answer(report.selected, rss_before, combo.steps.label)
        estimate = memory.forecast_memory(normalized, combo.size[0], combo.size[1], with_reference=combo.with_reference, memory=hardware.memory_snapshot(device), steps=combo.steps, device=device)
    except Exception as exc:  # noqa: BLE001 - recorded beside the combo; the run reports the same failure
        log.warning("forecast of %s failed: %s", combo.combo_id, exc)
        return {"error": f"{type(exc).__name__}: {exc}"}
    answer["full_device_peak"] = int(estimate["vram_bytes"])
    answer["host_growth"] = int(estimate["ram_bytes"])
    answer["full_host_peak"] = None
    answer["estimate_fits"] = bool(estimate["fits"])
    answer["estimate_calibrated"] = bool(estimate["calibrated"])
    answer["estimate_phases"] = estimate["phases"]
    log.info("FLUX.2 run forecast %s: device %.2f GiB, host +%.2f GiB (fits %s, calibrated %s)", combo.steps.label, answer["full_device_peak"] / GIB, answer["host_growth"] / GIB, estimate["fits"], estimate["calibrated"])
    return answer


def run_combo(service: Any, large_models: Any, capture: WorkCapture, combo: Flux2Combo, params: dict[str, Any], *, image_path: Path, out_png: Path, device: str, baseline_used: int | None) -> ComboOutcome:
    """One region edit with a `StepRecorder` installed as the service's phase observer (the phases'
    work from `capture`); never raises for a run failure (`oom` / `error`, logged with the traceback)."""
    import torch

    region_png, mask_png, _region_rgb = _region(image_path, combo.size)
    recorder = StepRecorder(device, baseline_used=baseline_used, status=large_models.status)
    rss_before = rss_private()
    planned = forecast_flux2(service, large_models, params, combo, device, rss_before)
    capture.clear()
    service.set_phase_observer(recorder)
    started = time.monotonic()
    outcome: ComboOutcome
    try:
        result = service.inpaint_image_bytes(region_png, mask_png, reference_bytes=region_png if combo.with_reference else None, params=params)
    except torch.OutOfMemoryError as exc:
        outcome = ComboOutcome(combo=combo, status="oom", error=str(exc), traceback=traceback.format_exc())
        log.error("combo %s: OUT OF MEMORY: %s", combo.combo_id, exc)
    except Exception as exc:  # noqa: BLE001 - recorded in the report and logged with traceback; the run goes on
        outcome = ComboOutcome(combo=combo, status="error", error=f"{type(exc).__name__}: {exc}", traceback=traceback.format_exc())
        log.exception("combo %s failed", combo.combo_id)
    else:
        from PIL import Image

        out_png.parent.mkdir(parents=True, exist_ok=True)
        out_png.write_bytes(result["image_png"])
        with Image.open(io.BytesIO(result["image_png"])) as image:
            mean, std = image_stats(image)
        outcome = ComboOutcome(combo=combo, status="ok", image_path=out_png, image_mean=mean, image_std=std, width=combo.size[0], height=combo.size[1], seed=params["seed"], vram_effective=result.get("vram_step_effective"), ram_effective=result.get("ram_step_effective"), degraded=bool(result.get("degraded")))
        log.info("combo %s: ok, steps %s/%s%s -> %s", combo.combo_id, outcome.vram_effective, outcome.ram_effective, " DEGRADED" if outcome.degraded else "", out_png)
    finally:
        service.set_phase_observer(None)
        recorder.close()
    capture.annotate(recorder.records)
    outcome.planned = planned
    outcome.phases = recorder.records
    outcome.rss_private_before = rss_before
    outcome.wall_s = time.monotonic() - started
    failed = [f"{record.model}:{record.step}" for record in recorder.records if record.failed or not record.ended]
    outcome.failed_phase = failed[-1] if failed else None
    return outcome


def run(parser: argparse.ArgumentParser, args: argparse.Namespace, *, repo_root: Path) -> int:
    """Run the subcommand; returns the process exit code (`parser.error` exits 2 on a usage error)."""
    try:
        combos = build_combos(args)
        out_dir = resolve_out(args.out, repo_root, args.dry_run, OUT_PREFIX)
        if args.vram_cap_gib is not None and not args.vram_cap_gib > 0:
            raise ValueError(f"--vram-cap-gib must be positive, not {args.vram_cap_gib}")
        for path in (args.transformer_path, args.vae_path, args.text_encoder_path, args.image):
            if not path.exists():
                raise ValueError(f"{path} does not exist")
    except ValueError as exc:
        parser.error(str(exc))
    setup_logging(out_dir)
    if args.dry_run:
        for index, combo in enumerate(combos, 1):
            print(f"{index:4d}  {combo.combo_id}")
        print(f"{len(combos)} combos; output: {out_dir or 'new ' + OUT_PREFIX + '* in the system temp dir'}")
        return 0
    assert out_dir is not None  # resolve_out creates one unless dry_run
    return _measure(args, combos, out_dir)


def _measure(args: argparse.Namespace, combos: list[Flux2Combo], out_dir: Path) -> int:
    """The GPU part of `run`: every combo cold, then the summary."""
    from modules.ai_backend.inpaint.flux2_klein import Flux2KleinInpaintService

    from .common import build_large_models, open_device
    from .report import MONOTONIC_TOLERANCE, Report

    option_device, device, cap, env = open_device(args.device, args.vram_cap_gib)
    if option_device is not None and option_device != device:
        log.warning("FLUX.2 runs on the service's own device choice; measuring %s", device)
    settings = {
        "driver": "flux2",
        "transformer_path": str(args.transformer_path),
        "vae_path": str(args.vae_path),
        "text_encoder_path": str(args.text_encoder_path),
        "image": str(args.image),
        "steps": args.steps,
        "seed": args.seed,
        "prompt": args.prompt,
        "monotonic_tolerance_bytes": MONOTONIC_TOLERANCE,
        "vram_cap": cap.as_dict(),
    }
    report = Report(out_dir, env, settings)
    report.write()
    large_models, small, _probes = build_large_models(None if args.vram_cap_gib is None else int(args.vram_cap_gib * 1024**3))
    capture = WorkCapture(large_models)
    service = Flux2KleinInpaintService(small, large_models=large_models)
    ran_any = False
    for index, combo in enumerate(combos, 1):
        log.info("[%d/%d] %s", index, len(combos), combo.combo_id)
        reset_device(service, device)
        if not args.keep_learned_floors:
            cleared = large_models.reset_floors()
            if cleared:
                log.info("forgot %d learned step floor(s) before this combo", cleared)
        outcome = run_combo(service, large_models, capture, combo, _params(args, combo, index), image_path=args.image, out_png=out_dir / "images" / f"{combo.combo_id}.png", device=device, baseline_used=cap.baseline_used)
        outcome.first_in_process = not ran_any and bool(outcome.phases)
        ran_any = ran_any or bool(outcome.phases)
        report.add(outcome)
    reset_device(service, device)
    report.log_summary()
    log.info("output directory: %s", out_dir)
    return 1 if report.failed() else 0
