"""
File: modules/ai_backend/inpaint/flux2_klein/test_service.py

Purpose:
Unit tests for `service.py` over a REAL large-model manager on the CPU
(`_ManagedRunFixture`: tiny real weights; only the diffusers pipeline call, the
tokenizer, the budget probes and the transport's device are faked).

Main responsibilities:
- the run: the pipeline handle is admitted and loaded before the encoder, the
  dilated latent mask and latents-only call, the composite, the guidance
  decision (`is_distilled`), the marks reference, `text_attention_in_mask`
  (one mask from the dilated mask, dropped after the call), every refusal that
  must happen before anything loads, the variant guard;
- the warm-up belongs to a cold load: a hot run reports load steps 0 and 9 only;
- a legacy request encodes under the CPU-pinned encoder spec, a two-axis one
  under the other (two handles, two keys, one status residency);
- a raising progress callback aborts the run and leaves no session in use;
- the response: legacy `applied` flags, the requested / effective steps and
  their legacy profile names, `degraded`;
- `status` (disk facts, residency from the large pool's parts, busy), `unload`;
- every `component_action` as a manager operation, and its refusals;
- the single-file transformer's byte-level progress (second wire level);
- the lock order of the two handles under the service lock (`LockOrderTests`):
  a run, an encoder «load» and a small-pool admission whose reclaim targets an
  idle FLUX.2 handle never deadlock, and no reclaim reaches the service lock.

Notes:
Module attributes are patched on the module that defines them (`hardware`,
`components`, `attention`), never on the `flux2_klein` package.
"""

from __future__ import annotations

import json
import shutil
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware, large_model
from modules.ai_backend.runtime.model_manager import SmallModelManager

from ._test_fixtures import _FakeEmbeds, _ManagedRunFixture, _TempTreeCase, _png_bytes

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]


def _region(side: int = 128) -> tuple[np.ndarray, np.ndarray]:
    region = np.full((side, side, 3), 90, dtype=np.uint8)
    mask = np.zeros((side, side), dtype=np.uint8)
    mask[48:80, 48:80] = 255
    return region, mask


class _RunCase(_ManagedRunFixture):
    """A managed run with the observer recording every manager step."""

    def setUp(self) -> None:
        super().setUp()
        self.steps: list[tuple[str, str, str]] = []
        self.service.set_phase_observer(lambda step, event, info: self.steps.append((str(info.get("model", "")), step, event)))
        self.frames: list[tuple[str, int, int, str, dict[str, Any]]] = []
        memory_patch = patch.object(hardware, "memory_snapshot", lambda *_a, **_k: {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0})
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def _progress(self, phase: str, step: int, total: int, label: str, **file_level: Any) -> None:
        self.frames.append((phase, step, total, label, file_level))

    def _run(self, *, region: np.ndarray | None = None, mask: np.ndarray | None = None, reference: np.ndarray | None = None, **overrides: object) -> dict[str, Any]:
        default_region, default_mask = _region()
        params = self.params(**{"prompt": "a cat", **overrides})
        return self.service.inpaint_image_bytes(
            _png_bytes(default_region if region is None else region, "RGB"),
            _png_bytes(default_mask if mask is None else mask, "L"),
            reference_bytes=None if reference is None else _png_bytes(reference, "RGB"),
            params=params,
            progress_callback=self._progress,
        )

    @property
    def call(self) -> dict[str, Any]:
        return self.pipeline_cls.calls[-1]


class InpaintRunTests(_RunCase):
    def test_the_pipeline_is_loaded_before_the_text_encoder_is_read(self) -> None:
        self._run()
        loads = [(model, step) for model, step, event in self.steps if event == "begin" and step.startswith("load:")]
        self.assertEqual(loads[:2], [("flux2_klein", "load:transformer"), ("flux2_klein", "load:vae")])
        # The fixture's request is legacy (no step fields): the CPU-pinned encoder spec.
        self.assertIn(("flux2_klein_text_encoder.cpu", "load:text_encoder"), loads)
        self.assertLess(loads.index(("flux2_klein", "load:vae")), loads.index(("flux2_klein_text_encoder.cpu", "load:text_encoder")))
        phases = [step for model, step, event in self.steps if event == "begin" and model == "flux2_klein" and step in ("denoise", "decode")]
        self.assertEqual(phases, ["denoise", "decode"])

    def test_a_legacy_request_encodes_on_the_cpu_and_a_two_axis_one_may_not(self) -> None:
        self._run(placement="full_gpu")
        self._run(placement="full_gpu", vram_step="max", ram_step="keep", prompt="a dog")
        encoders = {entry["model"]: entry for entry in self.large.status() if entry["model"].startswith("flux2_klein_text_encoder")}
        self.assertEqual(set(encoders), {"flux2_klein_text_encoder.cpu", "flux2_klein_text_encoder"})
        self.assertEqual(encoders["flux2_klein_text_encoder.cpu"]["components"]["text_encoder"]["exec_device"], "cpu")
        self.assertNotEqual(encoders["flux2_klein_text_encoder"]["key"], encoders["flux2_klein_text_encoder.cpu"]["key"])
        # The status residency of the encoder covers both handles.
        components = self.service.status(self.params())["components"]
        self.assertNotEqual(components["text_encoder"]["residency"], "not_loaded")

    def test_a_lower_vram_step_admits_the_encoder_under_its_combined_ceiling(self) -> None:
        # The ceiling is planned before the pipeline holds a reservation; the
        # pipeline is admitted uncapped, the encoder with the cap (`memory` notes).
        acquired: list[tuple[str, int | None]] = []
        real = self.large.acquire

        def spy(model: str, *args: Any, **kwargs: Any) -> Any:
            acquired.append((model, kwargs.get("device_cap_bytes")))
            return real(model, *args, **kwargs)

        with patch.object(self.large, "acquire", spy):
            self._run(vram_step="phase", ram_step="keep")
            self._run(vram_step="max", ram_step="keep", prompt="a dog")
        (pipeline, pipeline_cap), (encoder, encoder_cap) = acquired[:2]
        self.assertEqual((pipeline, pipeline_cap), ("flux2_klein", None))
        self.assertEqual(encoder, "flux2_klein_text_encoder")
        self.assertIsInstance(encoder_cap, int)
        self.assertGreaterEqual(encoder_cap, 0)
        # MAX has no higher step: no ceiling.
        self.assertEqual(acquired[2:], [("flux2_klein", None), ("flux2_klein_text_encoder", None)])

    def test_a_raising_progress_callback_aborts_the_run_and_leaves_nothing_in_use(self) -> None:
        class _PeerGone(RuntimeError):
            pass

        def explode(phase: str, step: int, total: int, label: str, **_file_level: Any) -> None:
            if phase == "generate" and step >= 1:
                raise _PeerGone("peer gone")

        region, mask = _region()
        with self.assertRaises(_PeerGone):
            self.service.inpaint_image_bytes(_png_bytes(region, "RGB"), _png_bytes(mask, "L"), params=self.params(prompt="a cat"), progress_callback=explode)
        self.assertEqual(len(self.pipeline_cls.calls), 1, "the run stopped inside the denoise")
        self.assertFalse(any(entry["in_use"] for entry in self.large.status()))
        self.assertEqual(self.service.health()["last_error"], "peer gone")
        # Run locks, reservations and leases were all returned: the next run is admitted.
        self.assertIn("image_png", self._run())

    def test_the_result_is_the_region_size_and_untouched_outside_the_mask(self) -> None:
        region, mask = _region()
        out = self._run(color_match=False)
        result = np.asarray(Image.open(__import__("io").BytesIO(out["image_png"])).convert("RGB"))
        self.assertEqual(result.shape, region.shape)
        self.assertTrue(np.array_equal(result[mask == 0], region[mask == 0]))

    def test_the_pipeline_is_asked_for_latents_and_a_dilated_mask(self) -> None:
        _region_rgb, mask = _region()
        self._run(mask_dilate_px=8)
        self.assertEqual(self.call["output_type"], "latent")
        self.assertIsNone(self.call.get("prompt", None))
        latent_mask = np.asarray(self.call["mask_image"])
        self.assertGreater(int((latent_mask > 0).sum()), int((mask > 0).sum()))

    @staticmethod
    def _steps(out: dict[str, Any]) -> tuple[Any, ...]:
        return tuple(out[key] for key in svc.STEP_FIELD_KEYS)

    def test_the_response_reports_the_steps(self) -> None:
        out = self._run(placement="encoder_cpu")
        self.assertEqual(self._steps(out), ("balanced", "balanced", "phase", "keep", "phase", "keep"))
        self.assertEqual((out["degraded"], out["degrade_reason"]), (False, None))
        self.assertTrue(out["applied"]["unload_transformer_before_vae"], "PHASE keeps the transformer off the device during the decode")
        out = self._run(placement="full_gpu")
        self.assertEqual(self._steps(out), ("max_speed", "max_speed", "max", "keep", "max", "keep"))
        self.assertFalse(out["applied"]["unload_transformer_before_vae"])
        out = self._run(placement="full_gpu", profile="max_economy")
        self.assertEqual(self._steps(out), ("max_economy", "max_economy", "min", "min", "min", "min"))
        out = self._run(placement="full_gpu", vram_step="half", ram_step="run")
        self.assertEqual(self._steps(out), ("economy", "economy", "half", "run", "half", "run"))
        self.assertTrue(out["applied"]["unload_transformer_before_vae"], "HALF parks the juggled transformer during the decode")
        out = self._run(placement="full_gpu", vram_step="auto")
        self.assertEqual(self._steps(out)[:4], ("auto", "max_speed", "auto", "auto"))

    def test_a_distilled_checkpoint_gets_no_raised_guidance_and_no_negative(self) -> None:
        self._run(guidance_scale=4.0)
        self.assertEqual(self.call["guidance_scale"], 1.0)
        self.assertIsNone(self.call["negative_prompt_embeds"])
        self.assertEqual([call["prompt"] for call in self.pipeline_cls.encode_calls], ["a cat"])

    def test_an_undeclared_checkpoint_still_gets_the_requested_guidance(self) -> None:
        self.declare_distilled(None)
        self._run(guidance_scale=4.0)
        self.assertEqual(self.call["guidance_scale"], 4.0)
        self.assertIsNotNone(self.call["negative_prompt_embeds"])
        self.assertEqual(sorted(call["prompt"] for call in self.pipeline_cls.encode_calls), ["", "a cat"])

    def test_a_marks_reference_reaches_the_pipeline_as_one_region_sized_image(self) -> None:
        region, _mask = _region()
        self._run(reference=np.full_like(region, 30))
        reference = self.call["image_reference"]
        self.assertIsInstance(reference, Image.Image)
        self.assertEqual(reference.size, (128, 128))

    def test_without_a_reference_the_pipeline_gets_no_image_reference(self) -> None:
        self._run()
        self.assertNotIn("image_reference", self.call)

    def test_without_text_attention_in_mask_the_call_carries_no_attention_kwargs(self) -> None:
        self._run()
        self.assertNotIn("attention_kwargs", self.call)

    def test_text_attention_in_mask_sends_one_mask_and_drops_it_after_the_call(self) -> None:
        seen: list[Any] = []
        original = self.pipeline_cls.__call__

        def call(pipe: Any, **kwargs: Any) -> Any:
            seen.append(pipe)
            return original(pipe, **kwargs)

        with patch.object(self.pipeline_cls, "__call__", call):
            self._run(text_attention_in_mask=True)
        mask = self.call["attention_kwargs"]["attention_mask"]
        self.assertEqual(mask.dim(), 2)
        self.assertIsNone(seen[-1]._attention_kwargs, "the pipeline's retained copy is dropped before the decode")

    def test_text_attention_in_mask_is_a_noop_under_whole_region(self) -> None:
        self._run(text_attention_in_mask=True, whole_region=True, mask=np.full((128, 128), 255, dtype=np.uint8))
        self.assertNotIn("attention_kwargs", self.call)

    def test_an_empty_mask_is_refused_before_anything_loads(self) -> None:
        with self.assertRaises(ValueError):
            self._run(text_attention_in_mask=True, mask=np.zeros((128, 128), dtype=np.uint8))
        self.assertEqual(self.large.status(), [])

    def test_a_reference_of_another_size_is_refused_before_anything_loads(self) -> None:
        with self.assertRaises(ValueError):
            self._run(reference=np.full((64, 64, 3), 30, dtype=np.uint8))
        self.assertEqual(self.large.status(), [])

    def test_a_mask_of_the_wrong_size_or_mode_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self._run(mask=np.zeros((64, 64), dtype=np.uint8))
        region, _mask = _region()
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(_png_bytes(region, "RGB"), _png_bytes(region, "RGB"), params=self.params(prompt="a cat"))
        self.assertEqual(self.large.status(), [])

    def test_a_region_the_pipeline_would_resize_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self._run(region=np.zeros((130, 128, 3), dtype=np.uint8), mask=np.zeros((130, 128), dtype=np.uint8))

    def test_whole_region_regenerates_everything_and_refuses_a_hole(self) -> None:
        solid = np.full((128, 128), 255, dtype=np.uint8)
        out = self._run(whole_region=True, mask=solid)
        self.assertEqual(out["region_size"], [128, 128])
        holed = solid.copy()
        holed[0:4, 0:4] = 0
        with self.assertRaises(ValueError):
            self._run(whole_region=True, mask=holed)

    def test_a_cold_run_warms_up_and_a_hot_one_does_not(self) -> None:
        self._run(placement="full_gpu")
        self.assertIn(svc.LOAD_STEP_WARMUP, [step for phase, step, *_rest in self.frames if phase == "load"])
        self.frames.clear()
        self._run(placement="full_gpu")
        self.assertEqual([step for phase, step, *_rest in self.frames if phase == "load"], [svc.LOAD_STEP_PREPARE, svc.LOAD_STEP_ENCODER_DONE])

    def test_an_unload_makes_the_next_run_cold_again(self) -> None:
        self._run()
        self.assertTrue(self.service.unload())
        self.frames.clear()
        self._run()
        self.assertIn(svc.LOAD_STEP_TRANSFORMER, [step for phase, step, *_rest in self.frames if phase == "load"])

    def test_unloading_the_encoder_after_encode_drops_its_handle(self) -> None:
        self._run(unload_text_encoder_after_encode=True)
        encoder = [held for key, held in self.held().items() if key.startswith("flux2_klein_text_encoder:")]
        self.assertTrue(all(sum(held) == 0 for held in encoder))
        self.assertEqual(self.frames[-1 - sum(1 for f in self.frames if f[0] == "generate")][3], "Текстовый энкодер выгружен")

    def test_the_prompt_cache_survives_an_unload(self) -> None:
        self._run()
        self.service.unload()
        self.pipeline_cls.encode_calls.clear()
        self._run()
        self.assertEqual(self.pipeline_cls.encode_calls, [])

    def test_a_negative_prompt_of_another_length_is_refused(self) -> None:
        self.declare_distilled(None)
        normalized = svc.normalize_flux2_klein_params(self.params(prompt="a cat", guidance_scale=4.0, text_attention_in_mask=True))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "a cat")] = torch.zeros((1, 512, 24))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "")] = torch.zeros((1, 256, 24))
        with self.assertRaisesRegex(ValueError, "негативного"):
            self._run(guidance_scale=4.0, text_attention_in_mask=True)


class VariantGuardTests(_RunCase):
    def _write_dimensions(self, hidden_size: int, joint_attention_dim: int) -> None:
        encoder_config = json.loads((self.root / "text_encoder" / "config.json").read_text(encoding="utf-8"))
        encoder_config["hidden_size"] = hidden_size
        (self.root / "text_encoder" / "config.json").write_text(json.dumps(encoder_config), encoding="utf-8")
        transformer_config = json.loads((self.root / "transformer" / "config.json").read_text(encoding="utf-8"))
        transformer_config["joint_attention_dim"] = joint_attention_dim
        (self.root / "transformer" / "config.json").write_text(json.dumps(transformer_config), encoding="utf-8")

    def test_a_mismatched_encoder_is_refused_before_any_weight_is_read(self) -> None:
        self._write_dimensions(4096, 7680)
        with self.assertRaises(ValueError) as caught:
            self._run()
        self.assertIn("7680", str(caught.exception))
        self.assertIn("12288", str(caught.exception))
        self.assertEqual(self.large.status(), [])
        self.assertEqual(self.pipeline_cls.encode_calls, [])

    def test_a_user_pressed_pipeline_load_is_guarded_too(self) -> None:
        self._write_dimensions(4096, 7680)
        with self.assertRaises(ValueError):
            self.service.component_action(self.params(), component="transformer", action="load")
        self.assertEqual(self.large.status(), [])


class SingleFileByteProgressTests(_RunCase):
    """A single-file transformer streams its bytes as the wire's SECOND progress level."""

    def setUp(self) -> None:
        super().setUp()
        single = self.root / "flux2-klein.safetensors"
        shutil.copyfile(self.root / "transformer" / "diffusion_pytorch_model.safetensors", single)
        self.paths["transformer_path"] = str(single)

    def test_the_bytes_arrive_as_file_fields_on_the_transformer_step(self) -> None:
        self._run()
        file_frames = [frame for frame in self.frames if frame[4]]
        self.assertTrue(file_frames)
        self.assertTrue(all(frame[1] == svc.LOAD_STEP_TRANSFORMER and frame[2] == svc.LOAD_PHASE_STEPS for frame in file_frames))
        self.assertTrue(all(frame[4]["file_label"] == "flux2-klein.safetensors" for frame in file_frames))
        last = file_frames[-1][4]
        self.assertEqual(last["file_step"], last["file_total"])

    def test_the_step_level_is_not_disturbed(self) -> None:
        self._run()
        steps = [frame[1] for frame in self.frames if frame[0] == "load" and not frame[4]]
        self.assertEqual(steps, list(range(10)))


class UnloadTests(_RunCase):
    def test_unload_reports_whether_anything_was_held(self) -> None:
        self.assertFalse(self.service.unload())
        self._run()
        self.assertTrue(self.service.unload())
        self.assertTrue(all(sum(held) == 0 for held in self.held().values()))
        self.assertFalse(self.service.unload())


class ComponentStatusTests(_RunCase):
    def _components(self, **overrides: object) -> dict[str, Any]:
        return self.service.status(self.params(**overrides))["components"]

    def test_an_idle_service_reports_every_component_as_not_loaded(self) -> None:
        for name, entry in self._components().items():
            if name in svc.ACTIONABLE_COMPONENTS:
                self.assertEqual((entry["residency"], entry["actions"]), ("not_loaded", ["load"]))
            else:
                self.assertNotIn("residency", entry)

    def test_a_loaded_pipeline_is_reported_per_component(self) -> None:
        self._run(placement="full_gpu")
        components_out = self._components(placement="full_gpu")
        self.assertEqual(components_out["transformer"]["residency"], "gpu")
        self.assertEqual(components_out["transformer"]["actions"], ["unload", "to_ram"])
        self.assertEqual(components_out["vae"]["actions"], ["unload", "warmup"])
        self.assertEqual(components_out["text_encoder"]["actions"], ["unload"])

    def test_a_busy_service_omits_the_residency_instead_of_waiting(self) -> None:
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
        self.assertNotIn("residency", out["components"]["transformer"])


class ComponentActionTests(_RunCase):
    def _action(self, component: str, action: str, **overrides: object) -> dict[str, Any]:
        return self.service.component_action(self.params(**overrides), component=component, action=action, progress_callback=self._progress)

    def test_unknown_names_are_request_errors(self) -> None:
        with self.assertRaises(ValueError):
            self._action("tokenizer", "load")
        with self.assertRaises(ValueError):
            self._action("vae", "explode")

    def test_an_action_outside_the_current_list_is_refused_naming_what_is_possible(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self._action("transformer", "to_ram")
        self.assertIn("«load»", str(caught.exception))

    def test_a_busy_service_refuses_instead_of_waiting(self) -> None:
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
        with self.assertRaisesRegex(RuntimeError, "занят"):
            self._action("transformer", "load")
        self.assertEqual(self.large.status(), [], "a refused action admitted nothing")

    def test_loading_the_transformer_builds_the_whole_pipeline_and_warms_it_up(self) -> None:
        out = self._action("transformer", "load", placement="full_gpu")
        self.assertTrue(out["performed"])
        self.assertEqual(out["components"]["vae"]["residency"], "gpu")
        self.assertEqual([frame[1] for frame in self.frames], list(range(7)))

    def test_parking_and_restoring_the_transformer(self) -> None:
        self._action("transformer", "load", placement="full_gpu")
        parked = self._action("transformer", "to_ram", placement="full_gpu")
        self.assertEqual(parked["components"]["transformer"]["residency"], "ram")
        self.assertEqual(parked["components"]["transformer"]["actions"], ["unload", "to_gpu"])
        restored = self._action("transformer", "to_gpu", placement="full_gpu")
        self.assertEqual(restored["components"]["transformer"]["residency"], "gpu")

    def test_unloading_the_vae_drops_the_pipeline_but_keeps_the_encoder(self) -> None:
        self._run(placement="full_gpu")
        out = self._action("vae", "unload", placement="full_gpu")
        self.assertEqual(out["components"]["transformer"]["residency"], "not_loaded")
        self.assertEqual(out["components"]["text_encoder"]["actions"], ["unload"])

    def test_loading_and_unloading_the_text_encoder_leave_the_pipeline_alone(self) -> None:
        loaded = self._action("text_encoder", "load")
        self.assertNotEqual(loaded["components"]["text_encoder"]["residency"], "not_loaded")
        self.assertEqual(loaded["components"]["transformer"]["residency"], "not_loaded")
        unloaded = self._action("text_encoder", "unload")
        self.assertEqual(unloaded["components"]["text_encoder"]["residency"], "not_loaded")

    def test_an_encoder_load_under_dropping_steps_keeps_it_anyway(self) -> None:
        for overrides in ({"profile": "max_economy"}, {"vram_step": "min", "ram_step": "run"}):
            with self.subTest(**overrides):
                loaded = self._action("text_encoder", "load", **overrides)
                self.assertEqual(loaded["components"]["text_encoder"]["residency"], "ram", "(PHASE, KEEP) keeps it in RAM")
                self._action("text_encoder", "unload")

    def test_the_encoder_load_steps(self) -> None:
        from modules.ai_backend.inpaint.flux2_klein import service as service_module
        from modules.ai_backend.runtime.large_models.tiers import RamStep, Steps, VramStep

        phase_keep = Steps(VramStep.PHASE, RamStep.KEEP)
        for requested, expected in (
            (Steps(VramStep.MAX, RamStep.MIN), Steps(VramStep.MAX, RamStep.MIN)),
            (Steps(VramStep.HALF, RamStep.KEEP), Steps(VramStep.HALF, RamStep.KEEP)),
            (Steps(VramStep.MIN, RamStep.AUTO), Steps(VramStep.MIN, RamStep.AUTO)),
            (Steps(VramStep.AUTO, RamStep.RUN), phase_keep),
            (Steps(VramStep.MIN, RamStep.MIN), phase_keep),
        ):
            with self.subTest(requested=requested):
                self.assertEqual(service_module._encoder_load_steps(requested), expected)

    def test_warming_up_the_vae_runs_one_tiny_decode(self) -> None:
        import diffusers

        self._action("transformer", "load", placement="full_gpu")
        decodes: list[tuple[int, ...]] = []
        real = diffusers.AutoencoderKLFlux2.decode

        def decode(vae: Any, latents: Any, return_dict: bool = True) -> Any:
            decodes.append(tuple(latents.shape))
            return real(vae, latents, return_dict=return_dict)

        with patch.object(diffusers.AutoencoderKLFlux2, "decode", decode):
            out = self._action("vae", "warmup", placement="full_gpu")
        self.assertTrue(out["performed"])
        self.assertEqual(len(decodes), 1)
        self.assertEqual(decodes[0][-1], svc.WARMUP_LATENT_CELLS)

    def test_a_failed_pipeline_load_action_leaves_nothing_held(self) -> None:
        import diffusers

        def explode(*_args: object, **_kwargs: object) -> object:
            raise OSError("disk read failed")

        with patch.object(diffusers.AutoencoderKLFlux2, "from_config", explode), self.assertRaises(OSError):
            self._action("transformer", "load", placement="full_gpu")
        self.assertTrue(all(sum(held) == 0 for held in self.held().values()), "the transformer read before the VAE failed is dropped")

    def test_a_failed_cold_load_drops_only_its_own_handle(self) -> None:
        import diffusers

        self._run(placement="full_gpu")
        warm = {key: held for key, held in self.held().items() if key.startswith("flux2_klein:") and sum(held) > 0}
        self.assertEqual(len(warm), 1)
        # Checkpoint B: a copy of the transformer under another path (another key, same model name).
        source = Path(self.paths["transformer_path"])
        other = source.with_name(f"other_{source.name}")
        if source.is_dir():
            shutil.copytree(source, other)
        else:
            shutil.copy2(source, other)

        def explode(*_args: object, **_kwargs: object) -> object:
            raise OSError("disk read failed")

        with patch.object(diffusers.AutoencoderKLFlux2, "from_config", explode), self.assertRaises(OSError):
            self._run(placement="full_gpu", transformer_path=str(other))
        held = self.held()
        for key, before in warm.items():
            self.assertEqual(held[key], before, "the warm handle of checkpoint A survives")
        failed = [value for key, value in held.items() if str(other) in key]
        self.assertTrue(all(sum(value) == 0 for value in failed), "checkpoint B's partial load is dropped")

    def test_a_pipeline_load_that_does_not_fit_is_refused(self) -> None:
        self.probes.device_total = 1 << 10
        self.probes.host_available = 1 << 10
        with self.assertRaises(RuntimeError):
            self._action("transformer", "load")
        self.assertTrue(all(sum(held) == 0 for held in self.held().values()))


class _RecordingLock:
    """`self._lock` stand-in: a real `RLock` that records which threads acquired it and whether inside a reclaim."""

    def __init__(self, in_reclaim: threading.local) -> None:
        self._inner = threading.RLock()
        self._in_reclaim = in_reclaim
        self.acquired_by: set[int] = set()
        self.acquired_in_reclaim = 0

    def acquire(self, blocking: bool = True, timeout: float = -1) -> bool:
        if getattr(self._in_reclaim, "active", False):
            self.acquired_in_reclaim += 1
        got = self._inner.acquire(blocking, timeout)
        if got:
            self.acquired_by.add(threading.get_ident())
        return got

    def release(self) -> None:
        self._inner.release()

    def __enter__(self) -> "_RecordingLock":
        self.acquire()
        return self

    def __exit__(self, *_exc: object) -> None:
        self.release()


class LockOrderTests(_RunCase):
    """The stated exception of the manager's "acquire before the service lock" rule (module notes of `service.py`).

    Three threads start together under a `Barrier`: a run that needs the
    encoder (pipeline session, then encoder session, under the service lock),
    a user-pressed encoder «load» (encoder session under the lock, or the
    "busy" refusal), and a small-pool admission whose hint is larger than the
    card, so its reclaim is offered every idle FLUX.2 component. None may
    deadlock (10 s join), and no reclaim may reach the service lock.
    """

    JOIN_S = 10.0
    ROUNDS = 3

    def setUp(self) -> None:
        super().setUp()
        self.in_reclaim = threading.local()
        self.lock = _RecordingLock(self.in_reclaim)
        self.service._lock = self.lock  # type: ignore[assignment]
        self.reclaimed: list[tuple[int, str]] = []
        real_execute = self.large._execute_offer
        fixture = self

        def execute(offer: Any) -> Any:
            fixture.reclaimed.append((threading.get_ident(), offer.key))
            fixture.in_reclaim.active = True
            try:
                return real_execute(offer)
            finally:
                fixture.in_reclaim.active = False

        execute_patch = patch.object(self.large, "_execute_offer", execute)
        execute_patch.start()
        self.addCleanup(execute_patch.stop)

    def _small_admission(self, key: str) -> None:
        from modules.ai_backend.runtime.residency.arbiter import ModelMemoryShortError
        from modules.ai_backend.runtime.residency.holdings import FootprintHint

        try:
            lease = self.small.begin_model_use(key, hint=FootprintHint("cuda:0", device_bytes=256 << 30, host_bytes=0))
        except ModelMemoryShortError:
            return
        lease.mark_load_failed()
        lease.release()

    def _prepare(self) -> None:
        """An idle pipeline on the device, no encoder: the run below needs both handles."""
        self._run(placement="full_gpu")
        self._action_unload_encoder()

    def _action_unload_encoder(self) -> None:
        self.service.component_action(self.params(), component="text_encoder", action="unload")

    def test_the_admission_alone_reclaims_an_idle_flux2_handle(self) -> None:
        self._prepare()
        self._small_admission("small-model")
        keys = {key for _thread, key in self.reclaimed}
        self.assertTrue(keys and all(key.startswith("flux2_klein") for key in keys), keys)
        self.assertEqual(self.lock.acquired_in_reclaim, 0)

    def test_run_encoder_load_and_reclaim_never_deadlock(self) -> None:
        # No subTest: a deadlocked round holds the service lock, so the next
        # round's preparation would block; the first failure must end the test.
        for round_index in range(self.ROUNDS):
            self._prepare()
            barrier = threading.Barrier(3)
            errors: list[BaseException] = []
            idents: dict[str, int] = {}

            def body(name: str, work: Any) -> None:
                idents[name] = threading.get_ident()
                barrier.wait(self.JOIN_S)
                try:
                    work()
                except (RuntimeError, ValueError) as exc:
                    # The encoder action refuses "busy" while the run holds the
                    # lock, or "not available" once the run has loaded the encoder.
                    if "занят" not in str(exc) and "недоступно" not in str(exc):
                        errors.append(exc)
                except BaseException as exc:  # noqa: BLE001 - reported below
                    errors.append(exc)

            works = {
                "run": lambda: self._run(placement="full_gpu", prompt=f"a dog {round_index}"),
                "encoder_load": lambda: self.service.component_action(self.params(), component="text_encoder", action="load"),
                "reclaim": lambda: self._small_admission(f"small-model-{round_index}"),
            }
            threads = [threading.Thread(target=body, args=(name, work), name=f"lock-order-{name}", daemon=True) for name, work in works.items()]
            # Daemon threads: a deadlock fails this test instead of hanging the suite at exit.
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(self.JOIN_S)
            self.assertFalse(any(thread.is_alive() for thread in threads), "deadlock: a thread did not finish within 10 s")
            self.assertEqual(errors, [])
            self.assertNotIn(idents["reclaim"], self.lock.acquired_by, "the reclaim thread took the service lock")
            self.assertEqual(self.lock.acquired_in_reclaim, 0, "a reclaim reached the service lock")
            self.lock.acquired_by.clear()


class LargeModelProfilesTests(_RunCase):
    """`large_model_profiles`: the pipeline's pickers planned with the encoder's admission claimed."""

    def _profiles(self, **overrides: object) -> tuple[dict[str, Any], list[tuple[str, dict[str, Any]]]]:
        from modules.ai_backend.runtime.large_models.tiers import AUTO_STEPS

        calls: list[tuple[str, dict[str, Any]]] = []
        real = self.large.profiles

        def spy(model: str, *args: Any, **kwargs: Any) -> Any:
            calls.append((model, dict(kwargs)))
            if model.startswith("flux2_klein_text_encoder") and self.encoder_does_not_fit:
                kwargs["claimed_host_bytes"] = kwargs["claimed_device_bytes"] = 1 << 60
            return real(model, *args, **kwargs)

        shape = large_model.request_shape(128, 128, with_reference=False, max_sequence_length=64, steps=1)
        with patch.object(self.large, "profiles", spy):
            answer = self.service.large_model_profiles(self.params(**{"prompt": "a cat", **overrides}), shape, format=None, steps=AUTO_STEPS)
        return answer, calls

    def setUp(self) -> None:
        super().setUp()
        self.encoder_does_not_fit = False

    def test_the_pipeline_is_planned_with_the_encoders_claim(self) -> None:
        answer, calls = self._profiles()
        self.assertEqual([model for model, _kwargs in calls], ["flux2_klein", "flux2_klein_text_encoder", "flux2_klein"])
        first, encoder, combined = (kwargs for _model, kwargs in calls)
        self.assertNotIn("claimed_device_bytes", first)
        self.assertGreater(encoder["claimed_device_bytes"] + encoder["claimed_host_bytes"], 0, "the encoder plans on what the pipeline left")
        self.assertGreater(combined["claimed_device_bytes"] + combined["claimed_host_bytes"], 0, "the pipeline answer counts the encoder")
        self.assertEqual(answer["model"], "flux2_klein")
        self.assertTrue(answer["selected"]["fits"])
        self.assertTrue(answer["encoder"]["fits"])
        self.assertIn("vram", answer)

    def test_the_float16_answer_names_its_registration(self) -> None:
        answer, calls = self._profiles(dtype="float16")
        self.assertEqual(answer["model"], "flux2_klein.float16")
        self.assertEqual(calls[0][0], "flux2_klein.float16")

    def test_a_cached_prompt_plans_the_pipeline_alone(self) -> None:
        self._run(prompt="a cat")
        answer, calls = self._profiles()
        self.assertEqual([model for model, _kwargs in calls], ["flux2_klein"])
        self.assertIsNone(answer["encoder"])

    def test_a_lower_vram_step_caps_the_encoder_at_the_higher_steps_combined_peak(self) -> None:
        from modules.ai_backend.runtime.large_models.tiers import RamStep, Steps, VramStep

        calls: list[tuple[str, dict[str, Any]]] = []
        real = self.large.profiles

        def spy(model: str, *args: Any, **kwargs: Any) -> Any:
            calls.append((model, dict(kwargs)))
            return real(model, *args, **kwargs)

        shape = large_model.request_shape(128, 128, with_reference=False, max_sequence_length=64, steps=1)
        with patch.object(self.large, "profiles", spy):
            answer = self.service.large_model_profiles(self.params(prompt="a cat"), shape, format=None, steps=Steps(VramStep.PHASE, RamStep.KEEP))
        encoder_calls = [kwargs for model, kwargs in calls if model.startswith("flux2_klein_text_encoder")]
        # The MAX cell of the chain (no ceiling of its own), then the selected PHASE cell under its ceiling.
        self.assertEqual([kwargs["steps"] for kwargs in encoder_calls], [Steps(VramStep.MAX, RamStep.KEEP), Steps(VramStep.PHASE, RamStep.KEEP)])
        self.assertIsNone(encoder_calls[0]["device_cap_bytes"])
        self.assertIsInstance(encoder_calls[1]["device_cap_bytes"], int)
        self.assertTrue(answer["encoder"]["fits"])

    def test_an_encoder_that_does_not_fit_makes_the_selection_refused(self) -> None:
        self.encoder_does_not_fit = True
        answer, _calls = self._profiles()
        self.assertFalse(answer["encoder"]["fits"])
        self.assertFalse(answer["selected"]["fits"])
        self.assertEqual(answer["selected"]["reason"], answer["encoder"]["reason"])
        self.assertIsNone(answer["selected"]["vram"])


class LargeModelInputsTests(_TempTreeCase):
    def test_the_adapter_answers_the_pipeline_options_and_the_device(self) -> None:
        with patch.object(hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:1"):
            options, device = svc.Flux2KleinInpaintService(SmallModelManager()).large_model_inputs(self.params(steps=4))
        self.assertIsInstance(options, svc.PipelineOptions)
        self.assertEqual((options.transformer_path, options.steps, device), (self.paths["transformer_path"], 4, "cuda:1"))
        with self.assertRaises(ValueError):
            svc.Flux2KleinInpaintService(SmallModelManager()).large_model_inputs({})

    def test_the_adapter_names_the_dtypes_registered_model(self) -> None:
        service = svc.Flux2KleinInpaintService(SmallModelManager())
        self.assertEqual(service.large_model_name(self.params()), "flux2_klein")
        self.assertEqual(service.large_model_name(self.params(dtype="float16")), "flux2_klein.float16")
        self.assertIn("flux2_klein.float16", service._large.models())


# ---------------------------------------------------------------------------
# Kept from the pre-manager suite: request shaping and the disk facts of `status`
# ---------------------------------------------------------------------------
class WholeRegionTests(_TempTreeCase):
    """The "no mask" mode: `whole_region` edits the entire validated region."""

    def test_it_is_off_by_default(self) -> None:
        self.assertFalse(
            svc.normalize_flux2_klein_params(self.params())["whole_region"]
        )

    def test_it_switches_off_the_dilate_and_the_color_match(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(whole_region=True, mask_dilate_px=32, color_match=True)
        )
        # Nothing to grow into, and no unchanged ring to take statistics from.
        self.assertEqual(normalized["mask_dilate_px"], 0)
        self.assertFalse(normalized["color_match"])

    def test_it_leaves_the_feather_alone(self) -> None:
        # The feather is what joins the regenerated region to the rest of the
        # page, so it is the one mask parameter this mode keeps.
        normalized = svc.normalize_flux2_klein_params(
            self.params(whole_region=True, mask_feather_px=20)
        )
        self.assertEqual(normalized["mask_feather_px"], 20)

    def test_a_solid_mask_is_accepted(self) -> None:
        svc._require_solid_mask(np.full((32, 32), 255, dtype=np.uint8))

    def test_a_mask_with_a_hole_is_refused_with_the_count(self) -> None:
        mask = np.full((32, 32), 255, dtype=np.uint8)
        mask[4:8, 4:8] = 0
        with self.assertRaises(ValueError) as caught:
            svc._require_solid_mask(mask)
        message = str(caught.exception)
        self.assertIn("16", message)
        self.assertIn("whole_region", message)

    def test_an_empty_mask_is_refused_too(self) -> None:
        with self.assertRaises(ValueError):
            svc._require_solid_mask(np.zeros((16, 16), dtype=np.uint8))


class StatusTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        torch_patch = patch.object(components, "is_torch_available", lambda: True)
        torch_patch.start()
        self.addCleanup(torch_patch.stop)
        memory_patch = patch.object(
            hardware,
            "memory_snapshot",
            lambda *_args, **_kwargs: {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0},
        )
        memory_patch.start()
        self.addCleanup(memory_patch.stop)
        self.service = svc.Flux2KleinInpaintService(SmallModelManager())

    def test_a_complete_tree_is_available(self) -> None:
        out = self.service.status(self.params())
        self.assertTrue(out["available"])
        self.assertIsNone(out["reason"])
        self.assertTrue(out["components"]["tokenizer"]["found"])
        self.assertTrue(out["components"]["scheduler"]["found"])
        self.assertFalse(out["loaded"])

    def test_guidance_supported_follows_the_checkpoints_own_declaration(self) -> None:
        # A plain boolean on the wire, not the tri-state behind it: the client
        # only needs to know whether to offer the slider, and the unknown case
        # reports `True` because that is what its run really does.
        for declared, expected in ((True, False), (False, True), (None, True)):
            with self.subTest(is_distilled=declared):
                self.declare_distilled(declared)
                self.assertIs(self.service.status(self.params())["guidance_supported"], expected)

    def test_an_unconfigured_service_reports_the_missing_component(self) -> None:
        out = self.service.status(None)
        self.assertFalse(out["available"])
        self.assertIn("энкодер", out["reason"])

    def test_a_missing_scheduler_is_named(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        out = self.service.status(self.params())
        self.assertFalse(out["available"])
        self.assertIn("планировщик", out["reason"])

    def _without_encoder(self, **overrides: object) -> dict[str, object]:
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _cache(self, params: dict[str, object]) -> None:
        """Put a stand-in embedding under the key a run would look up."""
        normalized = svc.normalize_flux2_klein_params(params)
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(str(normalized["prompt"]))

    def test_a_missing_encoder_is_reported_as_its_own_flag(self) -> None:
        out = self.service.status(self._without_encoder(prompt="a cat"))
        self.assertFalse(out["text_encoder_available"])
        # A complete tree still reports the flag, so the client can rely on it.
        self.assertTrue(self.service.status(self.params())["text_encoder_available"])

    def test_a_nonexistent_encoder_path_is_not_available_either(self) -> None:
        out = self.service.status(self.params(text_encoder_path=str(self.root / "gone")))
        self.assertFalse(out["text_encoder_available"])
        # The path itself is still reported, so a typo stays diagnosable — and the
        # reason names it AND the way out that needs no encoder.
        self.assertEqual(out["components"]["text_encoder"]["path"], str(self.root / "gone"))
        self.assertFalse(out["components"]["text_encoder"]["exists"])
        self.assertIn(str(self.root / "gone"), out["reason"])
        self.assertIn("кэш", out["reason"])

    def test_a_cached_prompt_keeps_the_service_available_without_an_encoder(self) -> None:
        params = self._without_encoder(prompt="a cat")
        self._cache(params)
        out = self.service.status(params)
        self.assertTrue(out["available"])
        self.assertIsNone(out["reason"])
        self.assertTrue(out["prompt_cached"])
        self.assertFalse(out["text_encoder_available"])

    def test_without_a_cached_prompt_the_reason_offers_the_cache(self) -> None:
        out = self.service.status(self._without_encoder(prompt="a cat"))
        self.assertFalse(out["available"])
        self.assertIn("энкодер", out["reason"])
        self.assertIn("кэш", out["reason"])

    def test_only_the_encoder_is_waived_by_a_cached_prompt(self) -> None:
        # The tokenizer is a real pipeline component (`_ensure_pipeline_locked`
        # builds the pipeline with one), so a cached prompt does NOT waive it —
        # a run without it fails at the load, not at the encode. Same for the
        # scheduler, which the denoise needs.
        import shutil

        params = self._without_encoder(prompt="a cat")
        self._cache(params)
        self.assertTrue(self.service.status(params)["available"])
        shutil.rmtree(self.root / "tokenizer")
        out = self.service.status(params)
        self.assertFalse(out["available"])
        self.assertIn("токенизатор", out["reason"])



if __name__ == "__main__":
    unittest.main()
