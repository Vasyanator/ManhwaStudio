"""
File: tools/large_model_calibration/flux3.py

Purpose:
The `flux3` subcommand: the FLUX.3 klein measurement matrix (variants x sizes
x cases x encoder modes x AE dtypes x the «Видеопамять» x «Оперативная
память» step grid) through the REAL `Flux3KleinService` under the real
large-model manager, plus the optional DiT parity check.

Key functions:
- `add_parser()` - the subcommand's options
- `run()` - parse, dry run or measure; exit code 0, 1 (a combo or the parity
  failed) or 2 (missing reference images)
- `build_matrix()`

Notes:
- `--dry-run` imports no torch (the flux3_klein package is torch-free at import).
- Size axis: `auto` (the default) passes width / height `None`, so the service
  picks the size (1024x1024 for `t2i`, the first reference's prepared size for
  edit / add cases); an explicit `WxH` applies to every case.
- `--vram-cap-gib` caps torch's allocator before the first allocation
  (`device_memory.apply_vram_cap`) AND is the manager's `Limits.vram_cap`
  (the planner treats the card as that large, context included), so a step
  pair that does not fit is refused or degraded instead of running into the
  allocator cap; the device baseline is read before ANY device call of this
  process, so the context size is measurable.
- `--vram-steps` / `--ram-steps` are the `Flux3Options.vram_step` /
  `ram_step` axes (default `auto` each); every pair of the two lists is a
  combo. The manager's learned floors are forgotten before every combo
  unless `--keep-learned-floors` (a degrade would otherwise lower every
  later plan).
- `--ref-max-pixels auto` caps every reference at the combo's explicit output
  area (`Flux3Options.reference_max_pixels`); `none` keeps the reference rule.
"""

from __future__ import annotations

import argparse
import logging
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path

from modules.ai_backend.inpaint.flux3_klein import AeDtype, Fp8Compute, WeightFormat, variant_availability
from modules.ai_backend.inpaint.flux3_klein.params import MIN_REFERENCE_PIXELS
from modules.ai_backend.runtime.large_models.tiers import RamStep, VramStep

from .cases import ALL, Case, missing_references, select_cases
from .common import parse_list, parse_sizes, resolve_out, setup_logging
from .measure import DEFAULT_ENCODER_LABELS, ENCODER_SETTINGS, Combo

log = logging.getLogger(__name__)

#: Environment variable that may name the model package root.
MODEL_ROOT_ENV = "MS_FLUX3_DIR"

#: Default axes.
DEFAULT_VARIANTS = "bf16,fp8r,nvfp4"
DEFAULT_SIZES = "auto"
DEFAULT_ENCODER_MODES = ",".join(DEFAULT_ENCODER_LABELS)
DEFAULT_AE_DTYPES = "float32"
DEFAULT_VRAM_STEPS = VramStep.AUTO.value
DEFAULT_RAM_STEPS = RamStep.AUTO.value

#: Output directory prefix in the system temp dir.
OUT_PREFIX = "lmc_flux3_"

#: The documented command for "does nvfp4 single-reference editing fit in 8 GB?"
#: (printed in `--help`).
EIGHT_GB_COMMAND = (
    "venv/bin/python tools/large_model_calibration flux3 --model-root <flux.3-klein root> --variants nvfp4 "
    "--sizes 512x512,768x768 --cases edit_kitchen_sage "
    "--vram-steps max,phase,half,min --ram-steps keep,run,min --ref-max-pixels auto --vram-cap-gib 8"
)

#: `--ref-max-pixels` value: `None` (reference caps), `"auto"` (the combo's
#: explicit output area), or a pixel count.
RefCap = int | str | None


@dataclass(frozen=True)
class Matrix:
    """The combos to run, in order, and the ones skipped with the reason."""

    combos: tuple[Combo, ...]
    skipped: tuple[tuple[str, str], ...]


def add_parser(subparsers: argparse._SubParsersAction) -> argparse.ArgumentParser:
    """Register the `flux3` subcommand (help text in English)."""
    parser = subparsers.add_parser(
        "flux3",
        help="measure FLUX.3 klein per step through the real service",
        description=(
            "FLUX.3 klein measurement through the real Flux3KleinService, per step, "
            "over variants x sizes x cases x encoder modes x AE dtypes x the VRAM x RAM step grid; optional DiT parity check."
        ),
        epilog=f"8 GB single-reference edit check: {EIGHT_GB_COMMAND}",
    )
    parser.add_argument("--model-root", type=Path, default=None, help=f"package root (or env {MODEL_ROOT_ENV})")
    parser.add_argument("--refs-root", type=Path, default=None, help="reference images root (default <model-root>/refs)")
    parser.add_argument("--out", type=Path, default=None, help=f"output dir (default: new {OUT_PREFIX}* in the system temp dir)")
    parser.add_argument("--variants", default=DEFAULT_VARIANTS, help=f"comma list of {[v.value for v in WeightFormat]}")
    parser.add_argument("--sizes", default=DEFAULT_SIZES, help="comma list of WxH or 'auto' (service default size)")
    parser.add_argument("--cases", default=ALL, help="comma list of case names, kinds (t2i, edit, add, multi) or 'all'")
    parser.add_argument(
        "--encoder-modes",
        default=DEFAULT_ENCODER_MODES,
        help=f"comma list of {list(ENCODER_SETTINGS)} (default {DEFAULT_ENCODER_MODES}; where the encoder lives is the steps' choice)",
    )
    parser.add_argument("--vram-steps", default=DEFAULT_VRAM_STEPS, help=f"comma list of {[step.value for step in VramStep]} (default {DEFAULT_VRAM_STEPS})")
    parser.add_argument("--ram-steps", default=DEFAULT_RAM_STEPS, help=f"comma list of {[step.value for step in RamStep]} (default {DEFAULT_RAM_STEPS})")
    parser.add_argument(
        "--ref-max-pixels",
        default="none",
        help="Flux3Options.reference_max_pixels: 'none' (reference caps, default), 'auto' (the output canvas "
        f"area of an explicit --sizes entry; none for 'auto' sizes), or a pixel count >= {MIN_REFERENCE_PIXELS}",
    )
    parser.add_argument(
        "--vram-cap-gib",
        type=float,
        default=None,
        help="treat the card as N GiB: the large-model manager's vram_cap (whole device usage, context included) "
        "plus torch's allocator cap (set_per_process_memory_fraction) as a hard backstop",
    )
    parser.add_argument("--ae-dtype", default=DEFAULT_AE_DTYPES, help=f"comma list of {[d.value for d in AeDtype]}")
    parser.add_argument("--fp8-compute", default=Fp8Compute.AUTO.value, choices=[c.value for c in Fp8Compute])
    parser.add_argument("--vl-ref-max-pixels", type=int, default=512 * 512, help="per-reference pixel cap shown to the VL encoder")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--steps", type=int, default=None, help="Euler steps (default: the package's sampler.steps)")
    parser.add_argument("--caption", default=None, help="override the caption of EVERY selected case")
    parser.add_argument("--device", default="discrete", help="'discrete' (never the iGPU), 'service' (General.ai_device), or cuda:N")
    parser.add_argument("--warm", action="store_true", help="keep weights resident between combos (no cold start)")
    parser.add_argument(
        "--keep-learned-floors",
        action="store_true",
        help="keep the manager's learned step floors between combos (default: forgotten before every combo, "
        "so each combo measures the steps it asks for)",
    )
    parser.add_argument("--parity", action="store_true", help="also run the DiT parity check (after the matrix)")
    parser.add_argument("--no-matrix", action="store_true", help="skip the matrix (with --parity: parity only)")
    parser.add_argument("--dry-run", action="store_true", help="print the matrix and exit (no torch import)")
    return parser


def _parse_ref_cap(value: str) -> RefCap:
    """`none` -> `None`, `auto` -> `"auto"`, digits -> that pixel count; `ValueError` otherwise."""
    token = value.strip()
    if token == "none":
        return None
    if token == "auto":
        return "auto"
    if not token.isdigit() or int(token) < MIN_REFERENCE_PIXELS:
        raise ValueError(f"--ref-max-pixels: {value!r} is not 'none', 'auto' or an integer >= {MIN_REFERENCE_PIXELS}")
    return int(token)


def _combo_ref_cap(ref_cap: RefCap, size: tuple[int, int] | None) -> int | None:
    """The `reference_max_pixels` of one combo: `auto` is the explicit output area (`None` for an `auto` size)."""
    if ref_cap == "auto":
        return None if size is None else size[0] * size[1]
    if ref_cap is None or isinstance(ref_cap, int):
        return ref_cap
    raise ValueError(f"unknown reference cap {ref_cap!r}")


def build_matrix(
    variants: list[WeightFormat],
    sizes: list[tuple[int, int] | None],
    cases: tuple[Case, ...],
    encoder_labels: list[str],
    ae_dtypes: list[AeDtype],
    availability: dict[WeightFormat, str | None],
    vram_steps: list[VramStep] | None = None,
    ram_steps: list[RamStep] | None = None,
    ref_cap: RefCap = None,
) -> Matrix:
    """All combos, variant-major (so `--warm` reuses a resident DiT), the step
    grid innermost (RAM step outer, VRAM step inner: each row of a grid runs
    from the fastest VRAM step down), each with its reference cap (`_combo_ref_cap`). A missing variant
    and a VL setting on a case without references are skipped with the
    reason."""
    combos: list[Combo] = []
    skipped: list[tuple[str, str]] = []
    for variant in variants:
        if availability.get(variant) is not None:
            skipped.append((variant.value, f"variant missing: {availability[variant]}"))
            continue
        for ae_dtype in ae_dtypes:
            for size in sizes:
                for case in cases:
                    for label in encoder_labels:
                        setting = ENCODER_SETTINGS[label]
                        for ram_step in ram_steps or [RamStep.AUTO]:
                            for vram_step in vram_steps or [VramStep.AUTO]:
                                combo = Combo(variant=variant, size=size, case=case, encoder=setting, ae_dtype=ae_dtype, vram_step=vram_step, ram_step=ram_step, ref_max_pixels=_combo_ref_cap(ref_cap, size))
                                if setting.needs_references and not case.references:
                                    skipped.append((combo.combo_id, "vl_refs needs references"))
                                    continue
                                combos.append(combo)
    return Matrix(combos=tuple(combos), skipped=tuple(skipped))


def _print_matrix(matrix: Matrix, out_dir: Path | None) -> None:
    """The dry-run listing on stdout (it IS the program's output)."""
    for index, combo in enumerate(matrix.combos, 1):
        refs = ", ".join(combo.case.references) or "-"
        print(f"{index:4d}  {combo.combo_id}  refs: {refs}")
    for what, reason in matrix.skipped:
        print(f"skip  {what}: {reason}")
    print(f"{len(matrix.combos)} combos, {len(matrix.skipped)} skipped; output: {out_dir or 'new ' + OUT_PREFIX + '* in ' + tempfile.gettempdir()}")


def run(parser: argparse.ArgumentParser, args: argparse.Namespace, *, repo_root: Path) -> int:
    """Run the subcommand; returns the process exit code (`parser.error` exits 2 on a usage error)."""
    model_root = args.model_root or (Path(os.environ[MODEL_ROOT_ENV]) if os.environ.get(MODEL_ROOT_ENV) else None)
    if model_root is None:
        parser.error(f"--model-root is required (or set {MODEL_ROOT_ENV})")
    model_root = model_root.expanduser().resolve()
    refs_root = (args.refs_root or model_root / "refs").expanduser().resolve()
    try:
        variants = parse_list(args.variants, {v.value: v for v in WeightFormat}, "--variants")
        sizes = parse_sizes(args.sizes)
        cases = select_cases(args.cases)
        encoder_labels = [str(label) for label in parse_list(args.encoder_modes, {k: k for k in ENCODER_SETTINGS}, "--encoder-modes")]
        ae_dtypes = parse_list(args.ae_dtype, {d.value: d for d in AeDtype}, "--ae-dtype")
        vram_steps = parse_list(args.vram_steps, {step.value: step for step in VramStep}, "--vram-steps")
        ram_steps = parse_list(args.ram_steps, {step.value: step for step in RamStep}, "--ram-steps")
        out_dir = resolve_out(args.out, repo_root, args.dry_run, OUT_PREFIX)
        ref_cap = _parse_ref_cap(args.ref_max_pixels)
        if args.vram_cap_gib is not None and not args.vram_cap_gib > 0:
            raise ValueError(f"--vram-cap-gib must be positive, not {args.vram_cap_gib}")
    except ValueError as exc:
        parser.error(str(exc))
    setup_logging(out_dir)

    availability = variant_availability(model_root)
    matrix = build_matrix(variants, sizes, cases, encoder_labels, ae_dtypes, availability, vram_steps=vram_steps, ram_steps=ram_steps, ref_cap=ref_cap)
    missing = missing_references(cases, refs_root) if not args.no_matrix else []
    for path in missing:
        log.error("reference image missing: %s", path)
    if args.dry_run:
        _print_matrix(matrix, out_dir)
        if args.vram_cap_gib is not None:
            print(f"VRAM cap: {args.vram_cap_gib} GiB (manager vram_cap + torch allocator cap; the device context is measured and reported separately)")
        return 1 if missing else 0
    if missing:
        return 2
    assert out_dir is not None  # resolve_out creates one unless dry_run
    return _measure(args, model_root, refs_root, out_dir, matrix)


def _measure(args: argparse.Namespace, model_root: Path, refs_root: Path, out_dir: Path, matrix: Matrix) -> int:
    """The GPU part of `run`: matrix, comparisons, parity, summary."""
    from modules.ai_backend.inpaint.flux3_klein import Flux3KleinService

    from .cases import CASES, load_references
    from .common import build_large_models, open_device
    from .measure import WorkCapture, reset_device, run_combo
    from .parity import run_parity
    from .report import MONOTONIC_TOLERANCE, Report, write_comparisons

    option_device, device, cap, env = open_device(args.device, args.vram_cap_gib)
    log.info("output %s", out_dir)
    settings = {
        "driver": "flux3",
        "model_root": str(model_root),
        "refs_root": str(refs_root),
        "fp8_compute": args.fp8_compute,
        "vl_ref_max_pixels": args.vl_ref_max_pixels,
        "seed": args.seed,
        "steps": args.steps,
        "caption_override": args.caption,
        "warm": args.warm,
        "keep_learned_floors": args.keep_learned_floors,
        "vram_steps": args.vram_steps,
        "ram_steps": args.ram_steps,
        "monotonic_tolerance_bytes": MONOTONIC_TOLERANCE,
        "ref_max_pixels": args.ref_max_pixels,
        "vram_cap": cap.as_dict(),
        "captions": {case.name: case.caption for case in CASES},
    }
    report = Report(out_dir, env, settings)
    report.skipped = [{"what": what, "reason": reason} for what, reason in matrix.skipped]
    for what, reason in matrix.skipped:
        log.warning("skipped %s: %s", what, reason)
    report.write()

    large_models, _small, _probes = build_large_models(None if args.vram_cap_gib is None else int(args.vram_cap_gib * 1024**3))
    capture = WorkCapture(large_models)
    service = Flux3KleinService(large_models)
    fp8_compute = Fp8Compute(args.fp8_compute)
    references_cache: dict[str, tuple] = {}
    combos = () if args.no_matrix else matrix.combos
    ran_any = False
    for index, combo in enumerate(combos, 1):
        log.info("[%d/%d] %s", index, len(combos), combo.combo_id)
        if not args.warm:
            reset_device(service, device)
        if not args.keep_learned_floors:
            # A degrade of an earlier combo records a floor that would lift
            # every later plan of the model to that step.
            cleared = large_models.reset_floors()
            if cleared:
                log.info("forgot %d learned step floor(s) before this combo", cleared)
        if combo.case.name not in references_cache:
            references_cache[combo.case.name] = load_references(combo.case, refs_root)
        outcome = run_combo(
            service,
            large_models,
            capture,
            combo,
            references_cache[combo.case.name],
            model_root=model_root,
            device=option_device,
            measure_device=device,
            fp8_compute=fp8_compute,
            vl_ref_max_pixels=args.vl_ref_max_pixels,
            caption=args.caption or combo.case.caption,
            seed=args.seed,
            steps=args.steps,
            image_path=out_dir / "images" / f"{combo.combo_id}.png",
            baseline_used=cap.baseline_used,
        )
        outcome.first_in_process = not ran_any and bool(outcome.phases)
        ran_any = ran_any or bool(outcome.phases)
        if outcome.status != "ok":
            reset_device(service, device)
        report.add(outcome)
    reset_device(service, device)
    if combos:
        write_comparisons(report)

    parity_failed = False
    if args.parity:
        parity_caption = args.caption or CASES[0].caption
        try:
            report.parity = run_parity(model_root, device, parity_caption, out_dir)
        except Exception as exc:  # noqa: BLE001 - recorded in the report and logged with traceback; exit code 1
            log.exception("parity check failed")
            report.parity = {"error": f"{type(exc).__name__}: {exc}"}
            parity_failed = True
        report.write()

    report.log_summary()
    log.info("output directory: %s", out_dir)
    return 1 if report.failed() or parity_failed else 0
