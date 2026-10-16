"""
File: tools/large_model_calibration/parity.py

Purpose:
`flux3 --parity`: one DiT forward on fixed inputs for every weight variant, compared
with the bf16 checkpoint, plus fp8r `_scaled_mm` against fp8r dequantization.

Main responsibilities:
- encode one caption once (TEXT mode encoder, then freed);
- build fixed 1024x1024 inputs: noise seed 0, image timestep t = 1, target
  timestep = the next flow-map time (`flow_map_schedule`);
- load each variant with `load_dit` (ONE DiT resident at a time), run the
  forward the way the sampler does (inference mode + bf16 autocast), keep the
  velocity on the host, free the DiT;
- metrics: max-abs, relative L2 and cosine, written to `parity.json`.

Key functions:
- `run_parity()`, `compare_outputs()`

Notes:
Uses the package-level building blocks (`load_dit`, `set_fp8_compute`,
`probe_scaled_mm`, `load_text_encoder`, `encode_text`, `pack`, ...), the same
ones the service calls; nothing of the pipeline is re-implemented here except
the one-step input assembly, which mirrors `Flux3KleinService._generate_locked`.
"""

from __future__ import annotations

import gc
import json
import logging
import time
from pathlib import Path
from typing import Any

from modules.ai_backend.inpaint import flux3_klein as fk

log = logging.getLogger(__name__)

GIB = 1024**3

#: Fixed parity canvas and noise seed.
PARITY_SIZE = 1024
PARITY_SEED = 0


def compare_outputs(candidate: Any, reference: Any) -> dict[str, float]:
    """max-abs, relative L2 (`|c - r| / |r|`) and cosine of two same-shape tensors (float64 on the CPU)."""
    a = candidate.double().flatten()
    b = reference.double().flatten()
    diff = a - b
    return {
        "max_abs": float(diff.abs().max()),
        "rel_l2": float(diff.norm() / b.norm().clamp(min=1e-30)),
        "cosine": float((a @ b) / (a.norm() * b.norm()).clamp(min=1e-30)),
    }


def _release(device: Any) -> None:
    """Collect and release the allocator cache of `device`."""
    import torch

    gc.collect()
    with torch.cuda.device(device):
        torch.cuda.empty_cache()


def _encode_caption(files: fk.ModelFiles, caption: str, device: Any) -> tuple[Any, Any, int]:
    """Encode `caption` with a TEXT-mode encoder, free it; returns host `(tokens, ids, real_length)`."""
    from modules.ai_backend.inpaint.flux3_klein import text_encoder

    handle = text_encoder.load_text_encoder(files.text_encoder_dir, mode=fk.EncoderMode.TEXT, device=device)
    encoded = text_encoder.encode_text(handle, caption)
    tokens, ids, real = encoded.tokens.cpu(), encoded.ids.cpu(), int(encoded.real_length)
    del handle, encoded
    _release(device)
    log.info("parity: caption encoded, %d tokens (%d real); encoder freed", tokens.shape[0], real)
    return tokens, ids, real


def _forward(dit: Any, package: fk.PackageConfig, text: tuple[Any, Any], device: Any) -> Any:
    """One DiT forward on the fixed inputs; returns the velocity as float32 on the CPU.

    Inputs mirror the service's first Euler step without references: noise from
    `initial_noise` (seed `PARITY_SEED`), scaled latent ids, image timestep 1,
    target timestep `schedule[1]`.
    """
    import torch

    from modules.ai_backend.inpaint.flux3_klein import autoencoder, packing, sampling

    dit_config = package.dit
    latent_h, latent_w = autoencoder.latent_shape(PARITY_SIZE, PARITY_SIZE)
    noise = packing.initial_noise(latent_h, latent_w, PARITY_SEED, dit_config.in_channels["image"]).to(device)
    image_ids = packing.scaled_ids(autoencoder.latent_ids(latent_h, latent_w, device), dit_config.image_rope_scale)
    packed = packing.pack([((text[0].to(device), text[1].to(device)), None, (noise, image_ids))], dit_config.in_channels)
    schedule = sampling.flow_map_schedule(package.sampler.steps, package.sampler.alpha)
    packed.timesteps["image"].fill_(schedule[0].item())
    target = schedule[1].reshape(1).to(device)
    with torch.inference_mode(), torch.autocast(torch.device(device).type, dtype=torch.bfloat16):
        velocity = dit(packed, timesteps_ref=target)
    torch.cuda.synchronize(device)
    return velocity.float().cpu()


def _measured(label: str, device: Any, body: Any) -> tuple[Any, dict[str, Any]]:
    """Run `body()` with the peak stats reset; returns its value and `{peak_alloc_gib, peak_reserved_gib, wall_s}`."""
    import torch

    torch.cuda.synchronize(device)
    torch.cuda.reset_peak_memory_stats(device)
    started = time.monotonic()
    value = body()
    torch.cuda.synchronize(device)
    stats = {
        "peak_alloc_gib": torch.cuda.max_memory_allocated(device) / GIB,
        "peak_reserved_gib": torch.cuda.max_memory_reserved(device) / GIB,
        "alloc_after_gib": torch.cuda.memory_allocated(device) / GIB,
        "wall_s": time.monotonic() - started,
    }
    log.info(
        "parity: %-28s peak alloc %6.2f GiB  peak reserved %6.2f GiB  %6.1f s",
        label,
        stats["peak_alloc_gib"],
        stats["peak_reserved_gib"],
        stats["wall_s"],
    )
    return value, stats


def run_parity(model_root: Path, device: str, caption: str, out_dir: Path) -> dict[str, Any]:
    """Run the parity check on `device` and write `out_dir/parity.json`; returns the report.

    Variants whose files are missing (`variant_availability`) are skipped and
    named in the report; comparisons needing a missing variant are omitted.

    # Raises
    Whatever loading or the forward raises (the caller records it).
    """
    import torch

    from modules.ai_backend.inpaint.flux3_klein import loading, quant

    torch_device = torch.device(device)
    availability = fk.variant_availability(model_root)
    report: dict[str, Any] = {
        "device": device,
        "size": PARITY_SIZE,
        "noise_seed": PARITY_SEED,
        "caption": caption,
        "skipped": {variant.value: reason for variant, reason in availability.items() if reason is not None},
        "runs": {},
        "metrics": {},
    }
    present = [variant for variant, reason in availability.items() if reason is None]
    if not present:
        raise FileNotFoundError(f"parity: no variant present under {model_root}")
    files0 = fk.resolve_model_files(model_root, present[0])
    tokens, ids, real = _encode_caption(files0, caption, torch_device)
    report["text_tokens"] = int(tokens.shape[0])
    report["text_real_tokens"] = real

    outputs: dict[str, Any] = {}
    for variant in (fk.WeightFormat.FP8R, fk.WeightFormat.BF16, fk.WeightFormat.NVFP4):
        if variant not in present:
            log.warning("parity: variant %s missing, skipped: %s", variant.value, availability[variant])
            continue
        files = fk.resolve_model_files(model_root, variant)
        package = fk.read_package_config(files.dit_config)
        loaded, load_stats = _measured(
            f"{variant.value} load",
            torch_device,
            lambda: loading.load_dit(files, package, variant=variant, device=torch_device, fp8_compute=fk.Fp8Compute.DEQUANT),
        )
        report["runs"][f"{variant.value}_load"] = load_stats
        dit = loaded.dit
        if variant is fk.WeightFormat.FP8R:
            label = "fp8r_dequant"
            outputs[label], report["runs"][label] = _measured(label, torch_device, lambda: _forward(dit, package, (tokens, ids), torch_device))
            probe = quant.probe_scaled_mm(torch_device)
            report["scaled_mm_probe"] = {"supported": probe.supported, "reason": probe.reason, "rel_error": probe.rel_error}
            if probe.supported:
                quant.set_fp8_compute(dit, fk.Fp8Compute.SCALED_MM)
                label = "fp8r_scaled_mm"
                outputs[label], report["runs"][label] = _measured(
                    label, torch_device, lambda: _forward(dit, package, (tokens, ids), torch_device)
                )
            else:
                log.warning("parity: rowwise _scaled_mm unsupported on %s (%s); no scaled_mm run", device, probe.reason)
        else:
            label = variant.value
            outputs[label], report["runs"][label] = _measured(label, torch_device, lambda: _forward(dit, package, (tokens, ids), torch_device))
        # Only one DiT resident at a time: drop every reference before the next load.
        del dit, loaded
        _release(torch_device)
        log.info("parity: %s freed, %.2f GiB still allocated", variant.value, torch.cuda.memory_allocated(torch_device) / GIB)

    pairs = (
        ("fp8r_scaled_mm", "fp8r_dequant"),
        ("fp8r_scaled_mm", "bf16"),
        ("fp8r_dequant", "bf16"),
        ("nvfp4", "bf16"),
    )
    for candidate, reference in pairs:
        if candidate in outputs and reference in outputs:
            metrics = compare_outputs(outputs[candidate], outputs[reference])
            report["metrics"][f"{candidate}_vs_{reference}"] = metrics
            log.info(
                "parity: %-32s max_abs %.4g  rel_l2 %.4g  cosine %.6f",
                f"{candidate} vs {reference}",
                metrics["max_abs"],
                metrics["rel_l2"],
                metrics["cosine"],
            )
    path = out_dir / "parity.json"
    path.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
    log.info("parity: written %s", path)
    return report
