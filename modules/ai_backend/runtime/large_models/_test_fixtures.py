"""
File: modules/ai_backend/runtime/large_models/_test_fixtures.py

Purpose:
Shared fixtures of the large-model core tests: header-only safetensors files
(the inventory reads headers only, so tensors may claim GiB without any data
bytes), a toy three-component spec (`dit` with blocks, `encoder`, `ae`) with
four phases, and budget builders.

Key functions:
- `write_header_only(path, tensors)` - a safetensors file whose header lists
  `(name, dtype, elements)` tensors back to back;
- `toy_spec(...)`, `toy_inventories(...)` - the toy model and its inventories
  for one or several formats;
- `simple_tables(...)` - a small calibration table with round numbers for the
  toy family, so forecast arithmetic can be asserted exactly;
- `budgets(...)` - `planner.Budgets` from GiB figures.

Notes:
Test-only; every file goes to a caller-owned temporary directory.
"""

from __future__ import annotations

import json
import struct
from collections.abc import Sequence
from pathlib import Path
from typing import Any

from ..memory_budget import DeviceBudget, HostBudget
from .cost_tables import CostTables, parse_tables
from .inventory import ComponentInventory, build_inventory
from .planner import Budgets, Reclaimable
from .spec import ComponentSpec, ExecDevice, FormatInfo, LargeModelSpec, PhaseSpec, PhaseWork, RequestShape, Role

GIB = 1024**3
MIB = 1024**2

_ITEM = {"BF16": 2, "F16": 2, "F32": 4, "U8": 1, "F8_E4M3": 1}


def write_header_only(path: Path, tensors: Sequence[tuple[str, str, int]]) -> Path:
    """Write a safetensors header listing `(name, dtype, elements)` tensors in order; no data bytes."""
    header: dict[str, Any] = {}
    offset = 0
    for name, dtype, elements in tensors:
        length = elements * _ITEM[dtype]
        header[name] = {"dtype": dtype, "shape": [elements], "data_offsets": [offset, offset + length]}
        offset += length
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload)
    return path


def _dit_block_of(key: str) -> str | None:
    parts = key.split(".")
    return f"blocks.{parts[1]}" if parts[0] == "blocks" else None


def _encoder_block_of(key: str) -> str | None:
    parts = key.split(".")
    return f"layers.{parts[1]}" if parts[0] == "layers" else None


def toy_files(directory: Path, weight_format: str, *, dit_block_gib: float = 1.0, blocks: int = 8, encoder_gib: float = 4.0, ae_gib: float = 0.25, dit_block_gibs: Sequence[float] | None = None) -> dict[str, list[Path]]:
    """Header-only files of the toy model for one format; `dit_block_gib` sets the DiT block size (`dit_block_gibs`: one size per block, residency order)."""
    directory.mkdir(parents=True, exist_ok=True)
    sizes = list(dit_block_gibs) if dit_block_gibs is not None else [dit_block_gib] * blocks
    dit = write_header_only(directory / f"dit_{weight_format}.safetensors", [("embed.w", "BF16", 64 * MIB // 2)] + [(f"blocks.{index}.w", "BF16", int(size * GIB) // 2) for index, size in enumerate(sizes)])
    layer_elements = int(encoder_gib * GIB) // 2 // 4
    encoder = write_header_only(directory / f"enc_{weight_format}.safetensors", [(f"layers.{index}.w", "BF16", layer_elements) for index in range(4)])
    ae = write_header_only(directory / f"ae_{weight_format}.safetensors", [("dec.w", "F32", int(ae_gib * GIB) // 4)])
    return {"dit": [dit], "encoder": [encoder], "ae": [ae]}


def _encode_work(shape: RequestShape, options: Any) -> PhaseWork:
    return PhaseWork(tokens=256 if shape.prompt_tokens is None else shape.prompt_tokens, variant="text")


def _denoise_work(shape: RequestShape, options: Any) -> PhaseWork:
    return PhaseWork(tokens=(shape.width // 16) * (shape.height // 16) + (256 if shape.prompt_tokens is None else shape.prompt_tokens), passes=4 if shape.steps is None else shape.steps)


def _decode_work(shape: RequestShape, options: Any) -> PhaseWork:
    return PhaseWork(pixels=shape.width * shape.height, variant="float32")


def toy_spec(files_by_format: dict[str, dict[str, list[Path]]], *, encoder_exec: frozenset[ExecDevice] = frozenset({ExecDevice.RUN_DEVICE, ExecDevice.CPU}), family: str = "toy") -> LargeModelSpec:
    """The toy model: `dit` (blocks), `encoder` (layers), `ae` (monolithic); phases encode, denoise, decode."""

    def _files(name: str) -> Any:
        return lambda options, weight_format: files_by_format[weight_format][name]

    return LargeModelSpec(
        family=family,
        components=(
            ComponentSpec("dit", Role.DIT, _files("dit"), _dit_block_of),
            ComponentSpec("encoder", Role.ENCODER, _files("encoder"), _encoder_block_of, exec_devices_allowed=encoder_exec),
            ComponentSpec("ae", Role.AE, _files("ae"), lambda key: None),
        ),
        phases=(
            PhaseSpec("encode", frozenset({"encoder"}), "encoder", _encode_work),
            PhaseSpec("denoise", frozenset({"dit"}), "dit", _denoise_work),
            PhaseSpec("decode", frozenset({"ae"}), "ae", _decode_work),
        ),
    )


def toy_inventories(spec: LargeModelSpec, files_by_format: dict[str, dict[str, list[Path]]]) -> dict[str, dict[str, ComponentInventory]]:
    """Inventories of every format of the toy model."""
    return {fmt: {component.name: build_inventory(component, component.files(None, fmt)) for component in spec.components} for fmt in files_by_format}


def toy_formats(**available: bool) -> dict[str, FormatInfo]:
    """`FormatInfo` for `big` (quality 2) and `small` (quality 1) toy formats."""
    return {
        "big": FormatInfo(available.get("big", True), None, 8 * GIB, "native", 2),
        "small": FormatInfo(available.get("small", True), None, 4 * GIB, "native", 1),
    }


def budgets(*, vram_free_gib: float, vram_total_gib: float = 32.0, ram_available_gib: float = 64.0, reserve_device_gib: float = 0.5, reserve_host_gib: float = 2.0, baseline_gib: float = 2.0, reclaim_device_gib: float = 0.0, reclaim_host_gib: float = 0.0, claimed_device_gib: float = 0.0) -> Budgets:
    """`Budgets` from GiB figures (rss = baseline)."""
    total = int(vram_total_gib * GIB)
    free = int(vram_free_gib * GIB)
    host = HostBudget(total=int(128 * GIB), available=int(ram_available_gib * GIB), cgroup_limit=None, cgroup_used=None, rss_anon=int(baseline_gib * GIB), rss_shmem=0, reserve=int(reserve_host_gib * GIB))
    device = DeviceBudget(device="cuda:0", total=total, free=free, torch_allocated=0, torch_reserved=0, non_torch=total - free, reserve=int(reserve_device_gib * GIB), cap=None)
    return Budgets(host=host, device=device, baseline_rss=int(baseline_gib * GIB), claimed_device_bytes=int(claimed_device_gib * GIB), reclaimable=Reclaimable(int(reclaim_device_gib * GIB), int(reclaim_host_gib * GIB)))


def _entry(phase: str, exec_kind: str, fixed_gib: float, *, family: str = "toy", variant: str = "*", measured: bool = True, seconds_per_ktoken: float = 0.0, seconds_per_mpixel: float = 0.0) -> dict[str, Any]:
    return {"family": family, "phase": phase, "variant": variant, "format": "*", "compute_path": "*", "exec": exec_kind, "measured": measured, "fixed_bytes": int(fixed_gib * GIB), "bytes_per_token": 0, "bytes_per_pixel": 0, "reserved_factor": 1.0, "seconds_fixed": 0.0, "seconds_per_ktoken_pass": seconds_per_ktoken, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": seconds_per_mpixel}


def simple_tables(*, calibrated: bool = True, overhead_gib: float = 0.0, margin_fraction: float = 0.0, denoise_measured: bool = True, encode_cpu_measured: bool = True) -> CostTables:
    """Round-number table: activations encode 0.25 / denoise 1.0 / decode 0.5 GiB (fixed), rates in GiB/s:
    park 1, restore 2, block H2D 4, disk cold 0.5, warm 8; prefetch depth 1; denoise 1 s per ktoken-pass.
    `denoise_measured` / `encode_cpu_measured` mark those entries unmeasured (per-phase calibration tests)."""
    data = {
        "device_class": "test",
        "calibrated": calibrated,
        "source": "test",
        "device": {"runtime_overhead_bytes": int(overhead_gib * GIB), "margin_fraction": margin_fraction, "margin_cap_bytes": GIB, "park_bytes_per_s": GIB, "restore_bytes_per_s": 2 * GIB, "block_h2d_bytes_per_s": 4 * GIB, "disk_cold_bytes_per_s": GIB // 2, "disk_warm_bytes_per_s": 8 * GIB, "prefetch_depth": 1},
        "phases": [
            _entry("encode", "device", 0.25),
            _entry("encode", "cpu", 0.25, measured=encode_cpu_measured),
            _entry("denoise", "device", 1.0, measured=denoise_measured, seconds_per_ktoken=1.0),
            _entry("decode", "device", 0.5, seconds_per_mpixel=1.0),
            _entry("*", "device", 3.0, family="*", measured=False),
            _entry("*", "cpu", 3.0, family="*", measured=False),
        ],
    }
    return parse_tables(data, source="test")
