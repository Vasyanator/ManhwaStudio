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
  `health` keys, the `inpaint.flux2_klein` response and its `applied` flag,
  the refusal of the retired placement wire, the `phase:"load"` step numbers,
  the `phase:"generate"` frames, mask semantics on a real run, the lease always
  being resolved, and the ORDER of the decode fallbacks. A failure here is a
  regression, never a reason to edit the test (protocol 14 retired the
  placement wire, `.estimate` and `status.memory` deliberately, and protocol 17
  moved the region post-processing to the client, so the answer is the RAW
  decode and the wire mask is the latent mask: those contracts were re-pinned
  with those changes).
- GOLDEN - a snapshot of today's NUMBERS: `forecast_memory` (the two-handle run
  forecast) for a fixed set of (step pair, region) cases, stored in
  `fixtures/forecast_golden.json`. These
  are EXPECTED TO CHANGE when the forecast is recalibrated (behaviour change
  B4: since WP6 they are the large-model manager's numbers). Update them
  deliberately, with the diff reviewed:
  `python -m modules.ai_backend.inpaint.flux2_klein.test_characterization --write-golden`
  from the repository root rewrites the file from the current code.

Main responsibilities:
- `ProgressNumberingContract`, `RetiredWireContract`, `StatusWireContract`,
  `HealthWireContract`, `InpaintRunContract`, `DecodeFallbackOrderContract` -
  the contracts;
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
  then a more economical «Видеопамять» step that takes the transformer off the device.
- What is already pinned elsewhere is referenced, not repeated: the wire
  codecs (`../test_region_imaging.py`), each decode rung on its own
  (`test_pipeline.py::DecodeRecoveryTests`), the lease protocol across services
  (`../test_lease_protocol.py`), the attention-mask formula (`test_attention.py`).
  Where a contract below overlaps one of those, it pins the WIRE-visible part
  only, because those files are rewritten by the refactor.
- Re-pinned with protocol 14: the cases name step pairs instead of the retired
  placements (the encoder is never CPU-pinned any more, so the encode figures
  of the former `full_gpu` / `encoder_cpu` cases moved onto the card), the
  `estimate` contract and the preset advice are gone (forecasts are
  `large_models.profiles`), and `status` carries no `memory`.
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
#: from the request (no encoder installed). Covers every concrete VRAM step,
#: every RAM step, «Авто», tiling engaging or not, the marks reference, the
#: attention mask, fp8 and the no-encoder case.
MAX_KEEP = {"vram_step": "max", "ram_step": "keep"}
PHASE_KEEP = {"vram_step": "phase", "ram_step": "keep"}
MIN_KEEP = {"vram_step": "min", "ram_step": "keep"}
FORECAST_CASES: dict[str, tuple[dict[str, Any], int, int, bool]] = {
    "max_keep_512": (MAX_KEEP, 512, 512, False),
    "max_keep_1024": (MAX_KEEP, 1024, 1024, False),
    "max_keep_512_reference": (MAX_KEEP, 512, 512, True),
    "max_keep_512_attention_mask": ({**MAX_KEEP, "text_attention_in_mask": True}, 512, 512, False),
    "max_keep_512_no_encoder": ({**MAX_KEEP, "__no_encoder__": True}, 512, 512, False),
    "phase_keep_512": (PHASE_KEEP, 512, 512, False),
    "phase_keep_1152x896_tiled": (PHASE_KEEP, 1152, 896, False),
    "phase_keep_1152x896_untiled": ({**PHASE_KEEP, "vae_tiling": False}, 1152, 896, False),
    "phase_keep_512_encoder_fp8": ({**PHASE_KEEP, "text_encoder_fp8": True}, 512, 512, False),
    "half_run_512": ({"vram_step": "half", "ram_step": "run"}, 512, 512, False),
    "min_keep_512": (MIN_KEEP, 512, 512, False),
    "min_keep_256x2048": (MIN_KEEP, 256, 2048, False),
    "min_min_512": ({"vram_step": "min", "ram_step": "min"}, 512, 512, False),
    "auto_auto_512": ({}, 512, 512, False),
    # «Энкодер на CPU»: the encoder never on the card, the RAM step places it.
    "max_keep_512_encoder_on_cpu": ({**MAX_KEEP, "encoder_on_cpu": True}, 512, 512, False),
    "phase_run_512_encoder_on_cpu": ({"vram_step": "phase", "ram_step": "run", "encoder_on_cpu": True}, 512, 512, False),
    "min_min_512_encoder_on_cpu": ({"vram_step": "min", "ram_step": "min", "encoder_on_cpu": True}, 512, 512, False),
}


def _request(paths: dict[str, str], overrides: dict[str, Any]) -> dict[str, Any]:
    """The wire `params` of one case: the tree's paths plus the case overrides.

    «Энкодер на CPU» is off unless a case turns it on: the other cases
    characterize the encoder placed by the steps.
    """
    params: dict[str, Any] = {**paths, "encoder_on_cpu": False}
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
    """Today's forecast for every golden case, as the JSON document."""
    forecasts: dict[str, Any] = {}
    with _golden_environment() as paths:
        for name, (overrides, width, height, with_reference) in FORECAST_CASES.items():
            normalized = svc.normalize_flux2_klein_params(_request(paths, overrides))
            forecasts[name] = memory.forecast_memory(normalized, width, height, with_reference=with_reference)
            assert set(forecasts[name]["breakdown"]) >= {"transformer", "text_encoder", "vae"}
    return {
        "_note": (
            "GOLDEN snapshot of FLUX.2 klein forecast_memory (klein 9B sizes), "
            "the large-model manager's forecast with the reference calibration table (its measured "
            "FLUX.2 entries). Expected to change with every recalibration of that table "
            "(behaviour change B4): regenerate deliberately with "
            "`python -m modules.ai_backend.inpaint.flux2_klein.test_characterization --write-golden` "
            "and review the diff. Case table: FORECAST_CASES in that module."
        ),
        "sizes": {"transformer": TRANSFORMER_BYTES, "text_encoder": TEXT_ENCODER_BYTES, "vae": VAE_BYTES},
        "forecast_memory": forecasts,
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


class RetiredWireContract(_TempTreeCase):
    """CONTRACT (protocol 14): the placement wire is refused by name; the kept flags keep their defaults.

    A stale client must get a clear error naming the field, never a silently
    ignored memory choice.
    """

    def test_every_retired_field_is_refused_by_name(self) -> None:
        for field in ("placement", "profile", "low_cpu_mem_usage", "vae_slicing", "unload_transformer_before_vae", "unload_text_encoder_after_encode"):
            with self.subTest(field=field):
                with self.assertRaisesRegex(ValueError, field):
                    svc.normalize_flux2_klein_params(self.params(**{field: False}))

    def test_the_persisted_flags_keep_their_defaults(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        self.assertEqual(
            {key: normalized[key] for key in ("vae_tiling", "text_encoder_fp8", "vram_step", "ram_step")},
            {"vae_tiling": True, "text_encoder_fp8": False, "vram_step": None, "ram_step": None},
        )


class StatusWireContract(_TempTreeCase):
    """CONTRACT: the `inpaint.flux2_klein.status` answer (`ipc/PROTOCOL.md` §5.4).

    Required keys are a SUBSET check: an added key is a deliberate wire change,
    a missing one is a regression. `memory` is gone since protocol 14 (free
    memory is `large_models.profiles`' answer).
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
        self.assertNotIn("memory", out)

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
    frames, the response shape, the raw answer and the residency are the
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
        self.file_frames: list[tuple[Any, ...]] = []

    def _progress(self, phase: str, step: int, total: int, label: str, **file_level: Any) -> None:
        # The byte level re-emits its step's frame with file fields: kept apart,
        # so `frames` is the step level the order contract talks about.
        if file_level:
            self.file_frames.append((phase, step, total, label, file_level))
            return
        self.frames.append((phase, step, total, label))

    def _run(self, **overrides: object) -> dict[str, Any]:
        return self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(prompt="a cat", **overrides),
            progress_callback=self._progress,
        )

    def test_the_response_shape(self) -> None:
        out = self._run(vram_step="phase", ram_step="keep")
        self.assertEqual(
            set(out),
            {"image_png", "region_size", "device", "applied", "oom_recovered", "text_attention_in_mask", "vram_step", "ram_step", "vram_step_effective", "ram_step_effective", "degraded", "degrade_reason"},
        )
        self.assertEqual(out["region_size"], [self.REGION, self.REGION])
        self.assertEqual(out["device"], "cuda:0")
        self.assertIs(out["oom_recovered"], False)
        self.assertIs(out["text_attention_in_mask"], False)
        # `applied` is what the Rust side persists (`Flux2AppliedFlags`): the
        # tiling flag alone since protocol 14.
        self.assertEqual(out["applied"], {"vae_tiling": True})

    def test_the_answer_is_the_raw_decode_over_the_wire_mask(self) -> None:
        # Protocol 17: the wire mask is the latent mask as the pipeline gets it,
        # and the answer is the decode itself (the fixture decodes to a solid
        # 210 over a solid-90 region), with no composite outside the mask.
        out = self._run(vram_step="max", ram_step="keep")
        result = np.asarray(Image.open(io.BytesIO(out["image_png"])).convert("RGB"))
        self.assertEqual(result.shape, self.region.shape)
        self.assertTrue(np.all(result == 210))
        self.assertTrue(np.array_equal(np.asarray(self.calls[-1]["mask_image"]), self.mask))

    def test_the_load_frames_follow_the_documented_order(self) -> None:
        self._run(vram_step="max", ram_step="keep")
        load = [step for phase, step, _total, _label in self.frames if phase == "load"]
        self.assertTrue(all(total == 9 for phase, _s, total, _l in self.frames if phase == "load"))
        # A cold run reports every load step, in this order, exactly once.
        self.assertEqual(load, list(range(10)))

    def test_byte_frames_ride_on_the_transformer_and_text_encoder_steps_only(self) -> None:
        self._run(vram_step="max", ram_step="keep")
        # `step` and `file_component` name the component being read (1 /
        # "transformer", 7 / "text_encoder"); the label is the step frame's own, the file label the
        # component's file or folder, and each read ends at its total.
        by_step: dict[int, list[dict[str, Any]]] = {}
        for phase, step, total, label, fields in self.file_frames:
            self.assertEqual((phase, total), ("load", 9))
            self.assertEqual(set(fields), {"file_step", "file_total", "file_label", "file_component"})
            by_step.setdefault(step, []).append(fields)
        self.assertEqual(sorted(by_step), [1, 7])
        self.assertEqual({fields["file_label"] for fields in by_step[1]}, {"transformer"})
        self.assertEqual({fields["file_label"] for fields in by_step[7]}, {"text_encoder"})
        self.assertEqual({fields["file_component"] for fields in by_step[1]}, {"transformer"})
        self.assertEqual({fields["file_component"] for fields in by_step[7]}, {"text_encoder"})
        for fields in by_step.values():
            self.assertEqual(fields[-1]["file_step"], fields[-1]["file_total"])

    def test_the_generate_frames_count_the_effective_steps(self) -> None:
        self._run(vram_step="max", ram_step="keep", steps=4, strength=1.0)
        generate = [(step, total) for phase, step, total, _label in self.frames if phase == "generate"]
        self.assertEqual(generate, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)])
        # Every load frame precedes the first generate frame.
        phases = [phase for phase, *_rest in self.frames]
        self.assertEqual(phases, sorted(phases, key=lambda phase: phase != "load"))

    def test_a_hot_second_run_jumps_from_preparation_to_the_last_step(self) -> None:
        self._run(vram_step="max", ram_step="keep")
        self.frames.clear()
        self._run(vram_step="max", ram_step="keep")
        load = [(step, label) for phase, step, _total, label in self.frames if phase == "load"]
        # Resident pipeline, cached prompt: nothing is loaded, and the bar still
        # reaches its last step so the client sees the load phase complete.
        self.assertEqual(load, [(0, "Подготовка запуска FLUX.2 klein"), (9, "Промпт взят из кэша")])

    def test_the_lease_is_resolved_and_the_pipeline_registered(self) -> None:
        manager = self.service._model_manager
        self._run(vram_step="max", ram_step="keep")
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
                self._run(vram_step="max", ram_step="keep")
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
    (3) a more economical «Видеопамять» step whose decode phase holds the transformer off
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
                params=self.params(prompt="a cat", vram_step="max", ram_step="keep", vae_tiling=False),
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
        # The transformer stayed on the device: the lowered tile was enough.
        self.assertEqual((outcome["vram_step_effective"], outcome["applied"]), ("max", {"vae_tiling": True}))


# ---------------------------------------------------------------------------
# GOLDEN
# ---------------------------------------------------------------------------
class ForecastGoldenTests(unittest.TestCase):
    """GOLDEN: today's `forecast_memory` answers.

    EXPECTED TO CHANGE with every recalibration of the reference table
    (behaviour change B4) — update deliberately with `--write-golden` and review the diff; never edit the
    JSON by hand to make a test pass. The case table is `FORECAST_CASES` above.
    """

    maxDiff = None

    def setUp(self) -> None:
        self.golden = _load_golden()
        self.current = _compute_golden()

    def test_the_case_tables_match_the_snapshot(self) -> None:
        self.assertEqual(set(self.golden["forecast_memory"]), set(FORECAST_CASES))
        self.assertEqual(self.golden["sizes"], self.current["sizes"])

    def test_forecast_memory_matches_the_snapshot(self) -> None:
        for name in FORECAST_CASES:
            with self.subTest(case=name):
                self.assertEqual(self.current["forecast_memory"][name], self.golden["forecast_memory"][name])


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
