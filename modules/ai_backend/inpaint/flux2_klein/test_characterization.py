"""
File: modules/ai_backend/inpaint/flux2_klein/test_characterization.py

Purpose:
Characterization tests of the FLUX.2 klein service, written BEFORE its resource
logic (placements, forecast, guard, park/restore, decode rungs) moves onto the
model-agnostic large-model manager. They are the "old behaviour" reference the
refactor is checked against.

Every test class here belongs to exactly ONE of two kinds, and says which in its
docstring:

- CONTRACT - a wire shape or an ordering the refactor must keep: the `status` /
  `estimate` / `health` keys, the `inpaint.flux2_klein` response and its
  `applied` flags, the accepted `placement` values, the `phase:"load"` step
  numbers, the `phase:"generate"` frames, mask semantics on a real run, the lease
  always being resolved, and the ORDER of the decode fallbacks. A failure here
  is a regression, never a reason to edit the test.
- GOLDEN - a snapshot of today's NUMBERS and choices: `forecast_memory` for a
  fixed set of (request, region) cases and `_preset_advice` for a fixed set of
  (request, free memory) cases, stored in `fixtures/forecast_golden.json`. These
  are EXPECTED TO CHANGE when the forecast is recalibrated (behaviour change
  B4: since WP6 they are the large-model manager's numbers). Update them
  deliberately, with the diff reviewed:
  `python -m modules.ai_backend.inpaint.flux2_klein.test_characterization --write-golden`
  from the repository root rewrites the file from the current code.

Main responsibilities:
- `ProgressNumberingContract`, `PlacementWireContract`, `StatusWireContract`,
  `EstimateWireContract`, `HealthWireContract`, `InpaintRunContract`,
  `DecodeFallbackOrderContract` - the contracts;
- `ForecastGoldenTests` - the golden snapshot, plus `_compute_golden` and the
  `--write-golden` entry point that share the case tables.

Notes:
- Harness (rebuilt in WP6, assertions unchanged except the two re-pins
  below): the run contracts drive a REAL large-model manager on the CPU over
  tiny real weights (`_ManagedRunFixture`: only the diffusers pipeline call,
  the tokenizer, the budget probes and the transport's device are faked); the
  other contracts use header-only weight files; no GPU. Patches go to the
  DEFINING module (`hardware`, `memory`), never to the package re-export.
- Re-pinned in WP6 (manager decision D1): the "pipeline registered" check reads
  the large pool (`LargeModelManager.status()`), because a large holding never
  counts in the small pool's counters; the decode fallback ORDER is the
  manager's ladder: tiling (where requested it engages), the lowered tile,
  then a more economical profile that takes the transformer off the device.
- What is already pinned elsewhere is referenced, not repeated: mask
  morphology and the composite (`test_imaging.py`), each decode rung on its own
  (`test_pipeline.py::DecodeRecoveryTests`), the lease protocol across services
  (`../test_lease_protocol.py`), the attention-mask formula (`test_attention.py`).
  Where a contract below overlaps one of those, it pins the WIRE-visible part
  only, because those files are rewritten by the refactor.
- The golden cases run against a temporary tree of header-only files with the
  real klein 9B file sizes and block structure (8 + 24 transformer blocks, 36
  encoder layers), planned with the reference calibration table, so the
  snapshot shows the numbers a user would see; the tree is deleted afterwards.
"""

from __future__ import annotations

import argparse
import io
import json
import sys
import tempfile
import types
import unittest
from collections.abc import Iterator
from contextlib import ExitStack, contextmanager
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

# Imported at module scope ON PURPOSE: the run tests patch `sys.modules` with
# `patch.dict`, which on exit removes every module first imported inside it. A
# first `import cv2` there (the mask dilation) would leave OpenCV half-registered
# and make every later import of it fail with `cv2.dnn has no DictValue`. cv2 is
# optional for this backend (`imaging.py` falls back to PIL), hence the guard.
try:
    import cv2  # noqa: F401 - imported for its side effect on `sys.modules`
except ImportError:
    cv2 = None

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware, memory
from modules.ai_backend.runtime.model_manager import SmallModelManager

from ._test_fixtures import (
    _ManagedRunFixture,
    _TempTreeCase,
    _make_model_tree,
    _png_bytes,
    _write_header_only,
)

#: The golden snapshot file. Tracked; regenerate only through `--write-golden`.
GOLDEN_PATH = Path(__file__).resolve().parent / "fixtures" / "forecast_golden.json"

#: klein 9B component sizes (the figures `test_memory.py` uses too), so the
#: golden numbers are the ones a user of the real checkpoint would see.
TRANSFORMER_BYTES = 18_157_185_168
TEXT_ENCODER_BYTES = 16_381_516_808
VAE_BYTES = 168_120_878

GIB = 1024**3

#: Golden forecast cases: name -> (request overrides, region width, height,
#: with_reference). `"__no_encoder__"` in the overrides drops `text_encoder_path`
#: from the request (no encoder installed). Twelve-plus pairs covering every
#: placement, the loader-dependent host term, tiling engaging or not, the marks
#: reference, the attention mask, fp8 and the encoder levers.
FORECAST_CASES: dict[str, tuple[dict[str, Any], int, int, bool]] = {
    "full_gpu_512": ({"placement": "full_gpu"}, 512, 512, False),
    "full_gpu_1024": ({"placement": "full_gpu"}, 1024, 1024, False),
    "full_gpu_512_reference": ({"placement": "full_gpu"}, 512, 512, True),
    "full_gpu_512_attention_mask": (
        {"placement": "full_gpu", "text_attention_in_mask": True},
        512,
        512,
        False,
    ),
    "full_gpu_512_no_encoder": ({"placement": "full_gpu", "__no_encoder__": True}, 512, 512, False),
    "encoder_cpu_512": ({"placement": "encoder_cpu"}, 512, 512, False),
    "encoder_cpu_512_low_cpu": ({"placement": "encoder_cpu", "low_cpu_mem_usage": True}, 512, 512, False),
    "encoder_cpu_1152x896_tiled": ({"placement": "encoder_cpu"}, 1152, 896, False),
    "encoder_cpu_1152x896_untiled": ({"placement": "encoder_cpu", "vae_tiling": False}, 1152, 896, False),
    "encoder_cpu_512_unload_encoder": (
        {"placement": "encoder_cpu", "unload_text_encoder_after_encode": True},
        512,
        512,
        False,
    ),
    "encoder_cpu_512_encoder_fp8": ({"placement": "encoder_cpu", "text_encoder_fp8": True}, 512, 512, False),
    "encoder_cpu_512_keep_transformer": (
        {"placement": "encoder_cpu", "unload_transformer_before_vae": False},
        512,
        512,
        False,
    ),
    "model_cpu_offload_512": ({"placement": "model_cpu_offload", "low_cpu_mem_usage": True}, 512, 512, False),
    "sequential_cpu_offload_512": (
        {"placement": "sequential_cpu_offload", "low_cpu_mem_usage": True},
        512,
        512,
        False,
    ),
    "sequential_cpu_offload_256x2048": (
        {"placement": "sequential_cpu_offload", "low_cpu_mem_usage": True},
        256,
        2048,
        False,
    ),
}

#: Golden preset-advice cases: name -> (request overrides, ram_free GiB,
#: vram_free GiB, with_reference). Region 512x512 throughout.
ADVICE_CASES: dict[str, tuple[dict[str, Any], float, float, bool]] = {
    "encoder_cpu_ram12_vram31": ({"placement": "encoder_cpu", "low_cpu_mem_usage": True}, 12.0, 31.9, False),
    "encoder_cpu_ram18_vram31": ({"placement": "encoder_cpu", "low_cpu_mem_usage": True}, 18.0, 31.9, False),
    "encoder_cpu_ram64_vram8": ({"placement": "encoder_cpu", "low_cpu_mem_usage": True}, 64.0, 8.0, False),
    "encoder_cpu_ram64_vram20": ({"placement": "encoder_cpu", "low_cpu_mem_usage": True}, 64.0, 20.0, False),
    "full_gpu_ram27_vram31": ({"placement": "full_gpu"}, 27.0, 31.9, False),
    "full_gpu_ram40_vram18_reference": ({"placement": "full_gpu"}, 40.0, 18.5, True),
    "custom_combination_ram30_vram31": (
        {"placement": "full_gpu", "low_cpu_mem_usage": True, "unload_transformer_before_vae": True},
        30.0,
        31.9,
        False,
    ),
    "unload_encoder_already_ram20_vram31": (
        {"placement": "encoder_cpu", "unload_text_encoder_after_encode": True},
        20.0,
        31.9,
        False,
    ),
    "nothing_fits_ram1_vram1": ({"placement": "encoder_cpu"}, 1.0, 1.0, False),
    "everything_fits_ram128_vram64": ({"placement": "encoder_cpu"}, 128.0, 64.0, False),
}


def _request(paths: dict[str, str], overrides: dict[str, Any]) -> dict[str, Any]:
    """The wire `params` of one case: the tree's paths plus the case overrides."""
    params: dict[str, Any] = dict(paths)
    for key, value in overrides.items():
        if key == "__no_encoder__":
            params.pop("text_encoder_path", None)
        else:
            params[key] = value
    return params


def _split(total_bytes: int, parts: int, rest_name: str, part_names: list[str], shape_tail: int = 4096) -> dict[str, tuple[str, tuple[int, ...]]]:
    """BF16 tensors: `parts` equal tensors named `part_names` plus `rest_name` taking the remainder, `total_bytes` in all."""
    elements = total_bytes // 2
    per_part = (elements // (parts + 1)) // shape_tail * shape_tail
    tensors: dict[str, tuple[str, tuple[int, ...]]] = {name: ("BF16", (per_part // shape_tail, shape_tail)) for name in part_names}
    tensors[rest_name] = ("BF16", (elements - per_part * parts,))
    return tensors


@contextmanager
def _golden_environment() -> Iterator[dict[str, str]]:
    """A temporary klein 9B-shaped tree of header-only files with the real file sizes; yields its three paths.

    Transformer: BFL layout, 8 double + 24 single blocks; encoder: 36 Qwen3
    layers plus embedding and head (causal-LM keys); VAE: one tensor. The
    reference calibration table is used whatever GPU the test host has, and the
    free memory of a case comes from the case itself. The tree, and every
    patch, is gone when the block exits.
    """
    from modules.ai_backend.runtime.large_models.cost_tables import REFERENCE_DEVICE_CLASS, load_tables

    with ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="flux2_golden_")))
        paths = _make_model_tree(root)
        blocks = [f"double_blocks.{index}.img_mlp.0.weight" for index in range(8)] + [f"single_blocks.{index}.linear1.weight" for index in range(24)]
        _write_header_only(Path(paths["transformer_path"]) / "diffusion_pytorch_model.safetensors", _split(TRANSFORMER_BYTES, len(blocks), "img_in.weight", blocks))
        head = 151936 * 4096 * 2
        layers = [f"model.layers.{index}.mlp.up_proj.weight" for index in range(36)]
        encoder = {"model.embed_tokens.weight": ("BF16", (151936, 4096)), "lm_head.weight": ("BF16", (151936, 4096))}
        encoder.update(_split(TEXT_ENCODER_BYTES - 2 * head, len(layers), "model.norm.weight", layers))
        _write_header_only(Path(paths["text_encoder_path"]) / "model.safetensors", encoder)
        _write_header_only(Path(paths["vae_path"]) / "diffusion_pytorch_model.safetensors", {"decoder.conv_in.weight": ("BF16", (VAE_BYTES // 2,))})
        reference = load_tables(REFERENCE_DEVICE_CLASS)
        stack.enter_context(patch.object(memory, "_tables_for", lambda _device: reference))
        stack.enter_context(patch.object(hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"))
        yield paths


def _compute_golden() -> dict[str, Any]:
    """Today's forecast and advice for every golden case, as the JSON document."""
    forecasts: dict[str, Any] = {}
    advice: dict[str, str] = {}
    with _golden_environment() as paths:
        for name, (overrides, width, height, with_reference) in FORECAST_CASES.items():
            normalized = svc.normalize_flux2_klein_params(_request(paths, overrides))
            forecasts[name] = memory.forecast_memory(normalized, width, height, with_reference=with_reference)
            assert set(forecasts[name]["breakdown"]) >= {"transformer", "text_encoder", "vae"}
        for name, (overrides, ram_free, vram_free, with_reference) in ADVICE_CASES.items():
            normalized = svc.normalize_flux2_klein_params(_request(paths, overrides))
            snapshot = {
                "ram_free": int(ram_free * GIB),
                "ram_total": int(128 * GIB),
                "vram_free": int(vram_free * GIB),
                "vram_total": int(64 * GIB),
            }
            advice[name] = memory._preset_advice(normalized, 512, 512, snapshot, with_reference=with_reference)
    return {
        "_note": (
            "GOLDEN snapshot of FLUX.2 klein forecast_memory / _preset_advice (klein 9B sizes), "
            "the large-model manager's forecast with the reference calibration table (its measured "
            "FLUX.2 entries). Expected to change with every recalibration of that table "
            "(behaviour change B4): regenerate deliberately with "
            "`python -m modules.ai_backend.inpaint.flux2_klein.test_characterization --write-golden` "
            "and review the diff. Case tables: FORECAST_CASES / ADVICE_CASES in that module."
        ),
        "sizes": {"transformer": TRANSFORMER_BYTES, "text_encoder": TEXT_ENCODER_BYTES, "vae": VAE_BYTES},
        "forecast_memory": forecasts,
        "preset_advice": advice,
    }


def _load_golden() -> dict[str, Any]:
    """The stored snapshot. A missing file is a FAILURE, not a skip: it is tracked."""
    return json.loads(GOLDEN_PATH.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# CONTRACTS
# ---------------------------------------------------------------------------
class ProgressNumberingContract(unittest.TestCase):
    """CONTRACT: the `phase:"load"` step numbers the Rust client counts against.

    The names are pinned symbolically all over the suite; the NUMBERS are what
    travel on the wire, so they are pinned literally here.
    """

    def test_the_load_steps_keep_their_numbers(self) -> None:
        self.assertEqual(
            [
                svc.LOAD_STEP_PREPARE,
                svc.LOAD_STEP_TRANSFORMER,
                svc.LOAD_STEP_TOKENIZER,
                svc.LOAD_STEP_VAE,
                svc.LOAD_STEP_SCHEDULER,
                svc.LOAD_STEP_PLACEMENT,
                svc.LOAD_STEP_WARMUP,
                svc.LOAD_STEP_TEXT_ENCODER,
                svc.LOAD_STEP_ENCODE,
                svc.LOAD_STEP_ENCODER_DONE,
            ],
            list(range(10)),
        )
        self.assertEqual(svc.LOAD_PHASE_STEPS, 9)

    def test_a_reporter_emits_the_four_field_frame(self) -> None:
        frames: list[tuple[Any, ...]] = []
        report = svc._progress_reporter(lambda *frame: frames.append(frame), "load", svc.LOAD_PHASE_STEPS)
        report(svc.LOAD_STEP_VAE, "Загрузка VAE")
        self.assertEqual(frames, [("load", 3, 9, "Загрузка VAE")])


class PlacementWireContract(_TempTreeCase):
    """CONTRACT (stage 1): the `placement` enum and its flags stay accepted on the wire.

    Stage 1 maps them onto the manager's profiles through an adapter; their
    removal is stage 2 (B2). An unknown value stays a request error.
    """

    def test_every_placement_value_is_accepted(self) -> None:
        self.assertEqual(
            tuple(svc.VALID_PLACEMENTS),
            ("full_gpu", "encoder_cpu", "model_cpu_offload", "sequential_cpu_offload"),
        )
        for placement in svc.VALID_PLACEMENTS:
            for low_cpu in (False, True):
                with self.subTest(placement=placement, low_cpu=low_cpu):
                    normalized = svc.normalize_flux2_klein_params(
                        self.params(
                            placement=placement,
                            low_cpu_mem_usage=low_cpu,
                            vae_tiling=True,
                            vae_slicing=False,
                            unload_transformer_before_vae=True,
                            unload_text_encoder_after_encode=True,
                            text_encoder_fp8=False,
                        )
                    )
                    self.assertEqual(normalized["placement"], placement)

    def test_an_unknown_placement_is_a_request_error(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(placement="gpu_please"))

    def test_the_persisted_flags_keep_their_defaults(self) -> None:
        # What a settings file written before a flag existed resolves to: the
        # Rust side mirrors these defaults (`MemoryPreset::values`).
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(self.params(placement=placement))
                self.assertEqual(
                    {
                        key: normalized[key]
                        for key in (
                            "low_cpu_mem_usage",
                            "vae_tiling",
                            "vae_slicing",
                            "unload_transformer_before_vae",
                            "unload_text_encoder_after_encode",
                            "text_encoder_fp8",
                        )
                    },
                    {
                        "low_cpu_mem_usage": False,
                        "vae_tiling": True,
                        "vae_slicing": True,
                        "unload_transformer_before_vae": placement != "full_gpu",
                        "unload_text_encoder_after_encode": False,
                        "text_encoder_fp8": False,
                    },
                )


class StatusWireContract(_TempTreeCase):
    """CONTRACT: the `inpaint.flux2_klein.status` answer (`ipc/PROTOCOL.md` §5.4).

    Required keys are a SUBSET check: an added key is a deliberate wire change,
    a missing one is a regression. The `memory` object stays in stage 1 (its
    removal is B2, stage 2).
    """

    SNAPSHOT = {"vram_total": 32 * GIB, "vram_free": 30 * GIB, "ram_total": 64 * GIB, "ram_free": 40 * GIB}

    def setUp(self) -> None:
        super().setUp()
        for module, name, replacement in (
            (components, "is_torch_available", lambda: True),
            (hardware, "memory_snapshot", lambda *_a, **_k: dict(self.SNAPSHOT)),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)
        self.service = svc.Flux2KleinInpaintService(SmallModelManager())

    def test_the_top_level_keys_and_their_types(self) -> None:
        out = self.service.status(self.params())
        expected_types = {
            "available": bool,
            "reason": type(None),
            "components": dict,
            "components_busy": bool,
            "memory": dict,
            "loaded": bool,
            "device": str,
            "prompt_cached": bool,
            "text_encoder_available": bool,
            "guidance_supported": bool,
        }
        self.assertLessEqual(set(expected_types), set(out))
        for key, kind in expected_types.items():
            with self.subTest(key=key):
                self.assertIsInstance(out[key], kind)
        self.assertEqual(out["device"], "cuda:0")
        self.assertEqual(out["memory"], self.SNAPSHOT)

    def test_the_component_entries(self) -> None:
        out = self.service.status(self.params())
        components_out = out["components"]
        self.assertEqual(set(components_out), {"text_encoder", "transformer", "vae", "tokenizer", "scheduler"})
        for name in ("text_encoder", "transformer", "vae"):
            with self.subTest(component=name):
                entry = components_out[name]
                self.assertLessEqual({"path", "exists", "size_bytes", "residency", "actions"}, set(entry))
                self.assertIsInstance(entry["size_bytes"], int)
                self.assertEqual(entry["residency"], "not_loaded")
                self.assertEqual(entry["actions"], ["load"])
        for name in ("tokenizer", "scheduler"):
            with self.subTest(component=name):
                self.assertEqual(set(components_out[name]), {"found", "path"})

    def test_the_residency_and_action_vocabularies(self) -> None:
        self.assertEqual(
            (svc.RESIDENCY_NOT_LOADED, svc.RESIDENCY_RAM, svc.RESIDENCY_GPU, svc.RESIDENCY_OFFLOADED, svc.RESIDENCY_MIXED),
            ("not_loaded", "ram", "gpu", "offloaded", "mixed"),
        )
        self.assertEqual(tuple(svc.COMPONENT_ACTIONS), ("load", "unload", "to_ram", "to_gpu", "warmup"))
        self.assertEqual(tuple(svc.ACTIONABLE_COMPONENTS), ("text_encoder", "transformer", "vae"))

    def test_a_busy_service_omits_residency_and_says_so(self) -> None:
        # The lock is held from another thread's point of view: `acquire(False)`
        # must fail, so it is taken by a helper thread for the call.
        import threading

        held, done = threading.Event(), threading.Event()

        def hold() -> None:
            with self.service._lock:
                held.set()
                done.wait(5)

        holder = threading.Thread(target=hold)
        holder.start()
        self.addCleanup(holder.join)
        self.addCleanup(done.set)
        self.assertTrue(held.wait(5))
        out = self.service.status(self.params())
        self.assertTrue(out["components_busy"])
        for name in svc.ACTIONABLE_COMPONENTS:
            self.assertNotIn("residency", out["components"][name])
            self.assertNotIn("actions", out["components"][name])


class EstimateWireContract(_TempTreeCase):
    """CONTRACT (stage 1): the `inpaint.flux2_klein.estimate` answer shape and `fits`.

    The NUMBERS are golden (B4); the keys, the types, the `breakdown` key set (the
    Rust side renders every unknown breakdown key as an extra row, so the set is
    exact) and the `fits` semantics are the contract.
    """

    BREAKDOWN_KEYS = {
        "transformer",
        "text_encoder",
        "vae",
        "activations",
        "peak_encode",
        "peak_denoise",
        "peak_decode",
    }

    def setUp(self) -> None:
        super().setUp()
        self.snapshot: dict[str, int] = {
            "vram_total": 12 << 30,
            "vram_free": 10 << 30,
            "ram_total": 32 << 30,
            "ram_free": 16 << 30,
        }
        self.snapshot_devices: list[Any] = []

        def snapshot(device: Any = None) -> dict[str, int]:
            self.snapshot_devices.append(device)
            return dict(self.snapshot)

        for module, name, replacement in (
            (hardware, "memory_snapshot", snapshot),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:1"),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)
        self.service = svc.Flux2KleinInpaintService(SmallModelManager())

    def _estimate(self, **overrides: object) -> dict[str, Any]:
        return self.service.estimate(params=self.params(**overrides), region_width=512, region_height=512)

    def test_the_keys_and_their_types(self) -> None:
        out = self._estimate()
        for key in ("vram_bytes", "ram_bytes", "vram_free", "ram_free"):
            with self.subTest(key=key):
                self.assertIsInstance(out[key], int)
        self.assertIsInstance(out["fits"], bool)
        self.assertEqual(set(out["breakdown"]), self.BREAKDOWN_KEYS)
        self.assertTrue(all(isinstance(value, int) for value in out["breakdown"].values()))
        self.assertEqual((out["vram_free"], out["ram_free"]), (10 << 30, 16 << 30))

    def test_the_attention_mask_is_a_breakdown_row_only_while_it_costs(self) -> None:
        self.assertEqual(
            set(self._estimate(text_attention_in_mask=True)["breakdown"]),
            self.BREAKDOWN_KEYS | {"attention_mask"},
        )
        self.assertEqual(
            set(self._estimate(text_attention_in_mask=True, whole_region=True)["breakdown"]),
            self.BREAKDOWN_KEYS,
        )

    def test_the_free_memory_is_read_from_the_card_the_run_would_use(self) -> None:
        self._estimate()
        self.assertEqual(self.snapshot_devices, ["cuda:1"])

    def test_unknown_free_memory_fits_and_a_tiny_one_does_not(self) -> None:
        self.snapshot = {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}
        self.assertTrue(self._estimate()["fits"])
        self.snapshot = {"vram_total": 1 << 20, "vram_free": 1 << 10, "ram_total": 1 << 20, "ram_free": 1 << 10}
        self.assertFalse(self._estimate()["fits"])

    def test_an_invalid_region_is_a_request_error(self) -> None:
        with self.assertRaises(ValueError):
            self.service.estimate(params=self.params(), region_width=100, region_height=256)


class HealthWireContract(unittest.TestCase):
    """CONTRACT: the FLUX.2 entry of the pushed health snapshot."""

    def test_the_idle_health_keys(self) -> None:
        with patch.object(hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"):
            out = svc.Flux2KleinInpaintService(SmallModelManager()).health()
        self.assertLessEqual({"ready", "model", "device", "active_key", "last_error"}, set(out))
        self.assertEqual(
            (out["ready"], out["model"], out["device"], out["active_key"], out["last_error"]),
            (False, "flux2_klein", "cuda:0", None, None),
        )


class InpaintRunContract(_ManagedRunFixture):
    """CONTRACT: one REAL `inpaint_image_bytes` run through the large-model manager.

    Nothing of the service or the manager is stubbed (`_ManagedRunFixture`:
    tiny real weights on the CPU, only the diffusers call, the tokenizer, the
    budget probes and the transport's device are faked) and the free-memory
    figures `status` reports are unknown, so the run order, the progress
    frames, the response shape, the composite and the residency are the
    production code's.
    """

    REGION = 128

    def setUp(self) -> None:
        super().setUp()
        memory_patch = patch.object(
            hardware,
            "memory_snapshot",
            lambda *_a, **_k: {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0},
        )
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

        side = self.REGION
        self.region = np.full((side, side, 3), 90, dtype=np.uint8)
        self.mask = np.zeros((side, side), dtype=np.uint8)
        self.mask[48:80, 48:80] = 255
        self.calls = self.pipeline_cls.calls
        self.frames: list[tuple[Any, ...]] = []

    def _progress(self, phase: str, step: int, total: int, label: str, **_file_level: Any) -> None:
        self.frames.append((phase, step, total, label))

    def _run(self, **overrides: object) -> dict[str, Any]:
        return self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(prompt="a cat", **overrides),
            progress_callback=self._progress,
        )

    def test_the_response_shape(self) -> None:
        out = self._run(placement="encoder_cpu", low_cpu_mem_usage=True)
        self.assertLessEqual(
            {"image_png", "region_size", "device", "placement", "applied", "oom_recovered", "text_attention_in_mask"},
            set(out),
        )
        self.assertEqual(out["region_size"], [self.REGION, self.REGION])
        self.assertEqual(out["device"], "cuda:0")
        self.assertEqual(out["placement"], "encoder_cpu")
        self.assertIs(out["oom_recovered"], False)
        self.assertIs(out["text_attention_in_mask"], False)
        # `applied` is persisted by the Rust side as ONE struct of exactly five
        # flags (`Flux2AppliedFlags`); a partial object is discarded there.
        self.assertEqual(
            out["applied"],
            {
                "unload_transformer_before_vae": True,
                "vae_tiling": True,
                "vae_slicing": True,
                "unload_text_encoder_after_encode": False,
                "text_encoder_fp8": False,
            },
        )

    def test_the_mask_is_a_permission_to_change(self) -> None:
        out = self._run(placement="full_gpu", color_match=False)
        result = np.asarray(Image.open(io.BytesIO(out["image_png"])).convert("RGB"))
        self.assertEqual(result.shape, self.region.shape)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(result[outside], self.region[outside]))
        self.assertFalse(np.array_equal(result[~outside], self.region[~outside]))

    def test_the_load_frames_follow_the_documented_order(self) -> None:
        self._run(placement="full_gpu")
        load = [step for phase, step, _total, _label in self.frames if phase == "load"]
        self.assertTrue(all(total == 9 for phase, _s, total, _l in self.frames if phase == "load"))
        # A cold run reports every load step, in this order, exactly once.
        self.assertEqual(load, list(range(10)))

    def test_the_generate_frames_count_the_effective_steps(self) -> None:
        self._run(placement="full_gpu", steps=4, strength=1.0)
        generate = [(step, total) for phase, step, total, _label in self.frames if phase == "generate"]
        self.assertEqual(generate, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)])
        # Every load frame precedes the first generate frame.
        phases = [phase for phase, *_rest in self.frames]
        self.assertEqual(phases, sorted(phases, key=lambda phase: phase != "load"))

    def test_a_hot_second_run_jumps_from_preparation_to_the_last_step(self) -> None:
        self._run(placement="full_gpu")
        self.frames.clear()
        self._run(placement="full_gpu")
        load = [(step, label) for phase, step, _total, label in self.frames if phase == "load"]
        # Resident pipeline, cached prompt: nothing is loaded, and the bar still
        # reaches its last step so the client sees the load phase complete.
        self.assertEqual(load, [(0, "Подготовка запуска FLUX.2 klein"), (9, "Промпт взят из кэша")])

    def test_the_lease_is_resolved_and_the_pipeline_registered(self) -> None:
        manager = self.service._model_manager
        self._run(placement="full_gpu")
        health = manager.health()
        self.assertEqual((health["loading_model_count"], health["active_model_count"]), (0, 0))
        # Re-pinned (D1): the pipeline is a LARGE holding, which the small pool's
        # counters never include; the large pool reports it, idle and holding bytes.
        pipeline = [entry for entry in self.large.status() if entry["model"] == "flux2_klein"]
        self.assertEqual(len(pipeline), 1)
        self.assertFalse(pipeline[0]["in_use"])
        self.assertGreater(pipeline[0]["device_bytes"] + pipeline[0]["host_bytes"], 0)
        self.assertTrue(self.service.health()["ready"])
        self.assertTrue(self.service.unload())
        self.assertTrue(all(held == (0, 0) for held in self.held().values()))

    def test_a_failed_load_resolves_the_lease_and_registers_nothing(self) -> None:
        import diffusers

        manager = self.service._model_manager

        def explode(*_args: object, **_kwargs: object) -> object:
            raise OSError("disk read failed")

        with patch.object(diffusers.AutoencoderKLFlux2, "from_config", explode):
            with self.assertRaises(OSError):
                self._run(placement="full_gpu")
        health = manager.health()
        self.assertEqual(
            (health["loading_model_count"], health["active_model_count"], health["resident_model_count"]),
            (0, 0, 0),
        )
        self.assertTrue(all(not entry["in_use"] for entry in self.large.status()))
        self.assertTrue(all(held == (0, 0) for held in self.held().values()))
        self.assertEqual(self.service.health()["last_error"], "disk read failed")


class DecodeFallbackOrderContract(_ManagedRunFixture):
    """CONTRACT: the decode out-of-memory fallbacks run in ONE fixed order.

    Re-pinned in WP6 (D1) to the large-model manager's ladder: (1) the VAE
    tiling fallback (it engages only where the region exceeds the VAE's
    threshold), (2) both tile thresholds lowered together for this decode,
    (3) a more economical profile whose decode phase holds the transformer off
    the device. The denoise is never repeated, the thresholds come back, and
    the ladder gives up after its retry cap with a named error.
    """

    def setUp(self) -> None:
        super().setUp()
        import diffusers
        import torch

        self.attempts: list[tuple[str, bool, tuple[int, int]]] = []
        self.oom_left = 0
        self.vaes: list[Any] = []
        real_decode = diffusers.AutoencoderKLFlux2.decode
        fixture = self

        def decode(vae: Any, latents: Any, return_dict: bool = True) -> Any:
            if latents.shape[-1] == svc.WARMUP_LATENT_CELLS:
                return real_decode(vae, latents, return_dict=return_dict)
            fixture.vaes.append(vae)
            fixture.attempts.append((fixture._transformer_place(), bool(vae.use_tiling), (vae.tile_latent_min_size, vae.tile_sample_min_size)))
            if fixture.oom_left > 0:
                fixture.oom_left -= 1
                raise torch.OutOfMemoryError("HIP out of memory. Tried to allocate 2.00 GiB")
            return real_decode(vae, latents, return_dict=return_dict)

        decode_patch = patch.object(diffusers.AutoencoderKLFlux2, "decode", decode)
        decode_patch.start()
        self.addCleanup(decode_patch.stop)

    def _transformer_place(self) -> str:
        key = next(entry["key"] for entry in self.large.status() if entry["model"] == "flux2_klein")
        device_bytes, _host = self.service._parts(key).get("transformer", (0, 0))
        return "cuda" if device_bytes > 0 else "cpu"

    def _run(self, width: int, height: int, *, oom_times: int) -> Any:
        self.oom_left = oom_times
        region = np.full((height, width, 3), 90, dtype=np.uint8)
        mask = np.zeros((height, width), dtype=np.uint8)
        mask[16:48, 16:48] = 255
        try:
            outcome: Any = self.service.inpaint_image_bytes(
                _png_bytes(region, "RGB"),
                _png_bytes(mask, "L"),
                params=self.params(prompt="a cat", placement="full_gpu", vae_tiling=False, vae_slicing=False),
            )
        except RuntimeError as exc:
            outcome = exc
        self.assertEqual(len(self.pipeline_cls.calls), 1, "the denoise output must not be re-derived")
        self.assertEqual((self.vaes[-1].tile_latent_min_size, self.vaes[-1].tile_sample_min_size), (128, 1024), "thresholds restored")
        return outcome

    def test_all_three_rungs_in_order_when_tiling_can_engage(self) -> None:
        # 1152 px is past the 1024 px threshold (an elongated region within 1 MP).
        outcome = self._run(1152, 896, oom_times=99)
        lowered = (svc.EMERGENCY_TILE_LATENT_MIN_SIZE, svc.EMERGENCY_TILE_LATENT_MIN_SIZE * 8)
        self.assertEqual(
            self.attempts,
            [
                ("cuda", False, (128, 1024)),
                ("cuda", True, (128, 1024)),
                ("cuda", True, lowered),
                ("cpu", True, lowered),
            ],
        )
        self.assertIsInstance(outcome, RuntimeError)

    def test_tiling_is_a_request_where_it_cannot_engage(self) -> None:
        # 768 px: the tiling rung sets the flag, but only the lowered tile can engage.
        outcome = self._run(768, 768, oom_times=99)
        lowered = (svc.EMERGENCY_TILE_LATENT_MIN_SIZE, svc.EMERGENCY_TILE_LATENT_MIN_SIZE * 8)
        self.assertEqual([attempt[0] for attempt in self.attempts], ["cuda", "cuda", "cuda", "cpu"])
        self.assertEqual([attempt[2] for attempt in self.attempts], [(128, 1024), (128, 1024), lowered, lowered])
        self.assertIsInstance(outcome, RuntimeError)

    def test_the_first_rung_that_fits_ends_the_ladder(self) -> None:
        outcome = self._run(1152, 896, oom_times=2)
        self.assertEqual(len(self.attempts), 3)
        self.assertTrue(outcome["oom_recovered"])
        self.assertTrue(outcome["degraded"])
        self.assertEqual(outcome["profile_effective"], "max_speed")
        # The transformer stayed on the device: the lowered tile was enough.
        self.assertEqual((outcome["applied"]["unload_transformer_before_vae"], outcome["applied"]["vae_tiling"]), (False, True))


# ---------------------------------------------------------------------------
# GOLDEN
# ---------------------------------------------------------------------------
class ForecastGoldenTests(unittest.TestCase):
    """GOLDEN: today's `forecast_memory` and `_preset_advice` answers.

    EXPECTED TO CHANGE with every recalibration of the reference table
    (behaviour change B4) — update deliberately with `--write-golden` and review the diff; never edit the
    JSON by hand to make a test pass. The case tables are `FORECAST_CASES` and
    `ADVICE_CASES` above.
    """

    maxDiff = None

    def setUp(self) -> None:
        self.golden = _load_golden()
        self.current = _compute_golden()

    def test_the_case_tables_match_the_snapshot(self) -> None:
        self.assertEqual(set(self.golden["forecast_memory"]), set(FORECAST_CASES))
        self.assertEqual(set(self.golden["preset_advice"]), set(ADVICE_CASES))
        self.assertEqual(self.golden["sizes"], self.current["sizes"])

    def test_forecast_memory_matches_the_snapshot(self) -> None:
        for name in FORECAST_CASES:
            with self.subTest(case=name):
                self.assertEqual(self.current["forecast_memory"][name], self.golden["forecast_memory"][name])

    def test_preset_advice_matches_the_snapshot(self) -> None:
        for name in ADVICE_CASES:
            with self.subTest(case=name):
                self.assertEqual(self.current["preset_advice"][name], self.golden["preset_advice"][name])


def _main(argv: list[str]) -> int:
    """`--write-golden`: rewrite `fixtures/forecast_golden.json` from the current code."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[1] if __doc__ else None)
    parser.add_argument("--write-golden", action="store_true", help="rewrite the golden snapshot file")
    args = parser.parse_args(argv)
    if not args.write_golden:
        parser.error("nothing to do: pass --write-golden (tests run under pytest)")
    GOLDEN_PATH.parent.mkdir(parents=True, exist_ok=True)
    GOLDEN_PATH.write_text(json.dumps(_compute_golden(), indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"written {GOLDEN_PATH}")  # noqa: T201 - CLI output of a developer tool
    return 0


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
