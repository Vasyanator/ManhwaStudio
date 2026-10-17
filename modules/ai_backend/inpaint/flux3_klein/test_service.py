"""
File: modules/ai_backend/inpaint/flux3_klein/test_service.py

Purpose:
Unit tests of `Flux3KleinService` over the real `LargeModelManager`
(`_service_test_fixtures.ServiceRig`): the two resource steps decide where
the encoder runs, the weight format and the steps are passed to the manager and
never changed, a memory failure degrades the run instead of failing it, a refusal
before / at the admission touches nothing, a failed load registers nothing, a
failed run keeps the handle and releases what its frames held, the reference
rules (VL images = AE images, the reference cap, the crop), `health()` while
the lock is held and read from the manager's ledger (a reclaim park seen live),
`unload`, a residency-key change, the key format; the step callback, «Энкодер
на CPU» (the encoder on the CPU at every VRAM step), a cancel that unloads
nothing, the `large_models.profiles` adapter and `status`.

Notes:
CPU only, no weights (stand-in modules in tiny real safetensors files); the
observer phase CONTRACT itself is pinned by `test_characterization.py`.
"""

from __future__ import annotations

import threading
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import torch
from PIL import Image

from ...runtime.large_models.manager import ComponentPlacement, LargeModelRefused
from ...runtime.large_models.spec import ExecDevice
from ...runtime.large_models.tiers import RamStep, Steps, Tier, VramStep
from ...runtime.residency.holdings import Action, Offer, OfferClass, Pool
from . import packing, quant, service
from ._service_test_fixtures import Recorder, ServiceRig
from .config import WeightFormat
from .params import AeDtype, EncoderMode, Flux3Options, Flux3Request, Fp8Compute
from .phases import Phase


class _Case(unittest.TestCase):
    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R, WeightFormat.NVFP4))
        self.service = self.rig.service
        self.recorder = Recorder()

    def _options(self, **overrides: Any) -> Flux3Options:
        return Flux3Options(model_root=self.rig.root, variant=overrides.pop("variant", WeightFormat.FP8R), **overrides)

    def _run(self, options: Flux3Options | None = None, request: Flux3Request | None = None) -> service.Flux3Result:
        self.recorder.events.clear()
        return self.service.generate(request or Flux3Request(caption="a cat"), options or self._options(), observer=self.recorder)

    def _session(self) -> Any:
        held = self.service._held
        assert held is not None
        return held.session


class ManagerWiringTests(_Case):
    def test_the_steps_decide_where_the_encoder_runs(self) -> None:
        cases = (
            (Steps(VramStep.MAX, RamStep.KEEP), ExecDevice.RUN_DEVICE),
            (Steps(VramStep.MIN, RamStep.KEEP), ExecDevice.CPU),
            # «Минимум» on both axes: no CPU encoder (it would need the RAM), juggled from the file.
            (Steps(VramStep.MIN, RamStep.MIN), ExecDevice.RUN_DEVICE),
        )
        for steps, expected in cases:
            with self.subTest(steps=steps.label):
                result = self._run(self._options(vram_step=steps.vram, ram_step=steps.ram))
                plan = self._session().plan
                self.assertEqual(plan.phase("encode").components["encoder"].exec_device, expected)
                self.assertEqual((result.applied.steps_requested, result.applied.steps_effective), (steps, steps))
                self.assertFalse(result.applied.degraded)

    def test_auto_reports_the_resolved_pair(self) -> None:
        result = self._run()
        self.assertEqual(result.applied.steps_requested, Steps(VramStep.AUTO, RamStep.AUTO))
        self.assertTrue(result.applied.steps_effective.concrete)
        info = self.recorder.begin(Phase.DENOISE)
        self.assertEqual((info["vram_step"], info["ram_step"]), ("auto", "auto"))
        self.assertEqual((info["vram_step_effective"], info["ram_step_effective"]), (result.applied.steps_effective.vram.value, result.applied.steps_effective.ram.value))

    def test_the_format_is_the_users_and_the_fp8_path_is_applied(self) -> None:
        result = self._run(self._options(variant=WeightFormat.NVFP4))
        self.assertEqual((result.applied.variant, result.applied.fp8_compute, result.applied.compute_path), (WeightFormat.NVFP4, None, "dequant"))
        self.assertEqual(self._session().weight_format, "nvfp4")
        self.rig.fp8 = Fp8Compute.SCALED_MM
        result = self._run(self._options(fp8_compute=Fp8Compute.SCALED_MM))
        self.assertEqual((result.applied.fp8_compute, result.applied.compute_path), (Fp8Compute.SCALED_MM, "scaled_mm"))
        self.assertEqual(self.recorder.begin(Phase.DENOISE)["compute_path"], "scaled_mm")

    def test_every_phase_carries_the_forecast_of_its_step(self) -> None:
        self._run()
        forecast = self._session().forecast
        denoise = self.recorder.begin(Phase.DENOISE)
        self.assertEqual(denoise["forecast_device_peak"], forecast.phases["denoise"].device_peak)
        restore = self.recorder.begin(Phase.DIT_RESTORE)
        self.assertEqual(restore["forecast_device_peak"], forecast.transitions["ref_encode->denoise"].device_peak)
        self.assertEqual(denoise["forecast_run_device_peak"], forecast.device_peak)

    def test_the_dit_runs_every_step_through_the_manager(self) -> None:
        self._run(request=Flux3Request(caption="a cat", steps=3))
        self.assertEqual(self.rig.dits[-1].calls, 3)

    def test_a_memory_failure_degrades_the_run_instead_of_failing_it(self) -> None:
        fastest = Steps(VramStep.MAX, RamStep.KEEP)
        self._run(self._options(vram_step=fastest.vram, ram_step=fastest.ram))
        dit = self.rig.dits[-1]
        original = type(dit).forward
        failures = [torch.OutOfMemoryError("CUDA out of memory. Tried to allocate 4096 B")]

        def flaky(module: Any, packed: Any, timesteps_ref: torch.Tensor) -> torch.Tensor:
            if failures:
                raise failures.pop()
            return original(module, packed, timesteps_ref)

        with patch.object(type(dit), "forward", flaky):
            result = self._run(self._options(vram_step=fastest.vram, ram_step=fastest.ram))
        self.assertTrue(result.applied.degraded)
        # A device failure walks the VRAM axis only; the request is kept as asked.
        self.assertEqual(result.applied.steps_requested, fastest)
        self.assertNotEqual(result.applied.steps_effective.vram, VramStep.MAX)
        self.assertIs(result.applied.steps_effective.ram, RamStep.KEEP)
        self.assertEqual(self.service.health()["degraded"], True)

    def test_an_admission_refusal_touches_nothing(self) -> None:
        self.rig.probes.device_total = 1024
        with self.assertRaises(LargeModelRefused):
            self._run(self._options(vram_step=VramStep.MIN, ram_step=RamStep.MIN))
        self.assertEqual(self.recorder.events, [])
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))
        self.assertIn("не помещается", self.service.health()["last_error"])

    def test_a_failed_load_registers_nothing(self) -> None:
        self.rig.fail_ae_build = True
        with self.assertRaises(ValueError):
            self._run()
        self.assertEqual(self.recorder.events[-1][:2], (Phase.AE_LOAD, "end"))
        self.assertTrue(self.recorder.events[-1][2]["failed"])
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))
        health = self.service.health()
        self.assertEqual((health["loaded"], health["ready"], health["last_error"]), (False, False, "bad autoencoder"))

    def test_a_failed_run_keeps_the_handle_and_frees_what_its_frames_held(self) -> None:
        self._run()
        self.rig.dits[-1].fail = True
        released: list[str] = []
        with patch.object(service, "_clear_torch_cache", lambda: released.append("cache")), self.assertRaises(RuntimeError):
            self._run()
        self.assertEqual(released, ["cache"])
        self.assertEqual(self.rig.large_holdings(), (1, 0, 0))
        self.assertTrue(self.service.health()["ready"])
        self.rig.dits[-1].fail = False
        self._run()
        self.assertIsNone(self.service.health()["last_error"])

    def test_an_explicit_scaled_mm_that_cannot_run_fails_before_the_admission(self) -> None:
        def refuse(requested: Fp8Compute, device: Any) -> Fp8Compute:
            raise RuntimeError("нет ядра")

        with patch.object(quant, "resolve_fp8_compute", refuse), self.assertRaises(RuntimeError):
            self._run(self._options(fp8_compute=Fp8Compute.SCALED_MM))
        self.assertEqual(self.recorder.events, [])
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))

    def test_refusals_happen_before_the_admission(self) -> None:
        too_many = Flux3Request(caption="x", references=tuple(Image.new("RGB", (32, 32)) for _ in range(11)))
        cases = (
            (self._options(), too_many),
            (self._options(encoder_mode=EncoderMode.VL_REFS), Flux3Request(caption="no refs")),
            (self._options(), Flux3Request(caption="   ")),
        )
        for options, request in cases:
            with self.subTest(request=request.caption), self.assertRaises(ValueError):
                self._run(options, request)
        with self.assertRaises(FileNotFoundError):
            self._run(self._options(variant=WeightFormat.BF16))
        self.assertEqual(self.recorder.events, [])
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))
        self.assertEqual(self.rig.dits, [])


class ReferenceTests(_Case):
    def setUp(self) -> None:
        super().setUp()
        self.ae_images: list[tuple[int, ...]] = []
        self.vl_images: list[Any] = []
        original = packing.to_tensor

        def to_tensor(image: Any) -> torch.Tensor:
            tensor = original(image)
            self.ae_images.append(tuple(tensor.shape))
            return tensor

        def encode_with_references(handle: Any, caption: str, references: Any, *, vl_ref_max_pixels: int) -> Any:
            self.vl_images.extend(references)
            from ._service_test_fixtures import encoded

            return encoded()

        from . import text_encoder

        for owner, name, stub in ((packing, "to_tensor", to_tensor), (text_encoder, "encode_with_references", encode_with_references)):
            patcher = patch.object(owner, name, stub)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_vl_refs_shows_the_encoder_the_images_the_ae_receives(self) -> None:
        references = (Image.new("RGB", (64, 48), (200, 0, 0)), Image.new("RGB", (32, 32), (0, 200, 0)))
        result = self._run(self._options(encoder_mode=EncoderMode.VL_REFS, vl_ref_max_pixels=4096), Flux3Request(caption="two things", references=references))
        self.assertEqual([(3, image.height, image.width) for image in self.vl_images], self.ae_images)
        info = self.recorder.begin(Phase.REF_ENCODE)
        self.assertEqual((info["n_refs"], info["ref_tokens"]), (2, 4 * 3 + 2 * 2))
        self.assertEqual((result.width, result.height), (64, 48))
        self.assertEqual(self._session().model, "flux3_klein.vl_refs.ae_float32")

    def test_reference_max_pixels_caps_what_the_ae_and_the_vl_encoder_see(self) -> None:
        self._run(
            self._options(encoder_mode=EncoderMode.VL_REFS, reference_max_pixels=96 * 64),
            Flux3Request(caption="small", references=(Image.new("RGB", (256, 128), (200, 0, 0)),), width=64, height=64),
        )
        (image,) = self.vl_images
        self.assertLessEqual(image.width * image.height, 96 * 64)
        self.assertEqual(self.ae_images, [(3, image.height, image.width)])
        info = self.recorder.begin(Phase.DENOISE)
        self.assertEqual((info["reference_max_pixels"], info["ref_tokens"]), (96 * 64, (image.width // 16) * (image.height // 16)))

    def test_a_single_padded_reference_is_cropped_back(self) -> None:
        result = self._run(request=Flux3Request(caption="edit", references=(Image.new("RGB", (40, 64)),)))
        self.assertEqual(self.recorder.begin(Phase.DENOISE)["width"], 64)
        self.assertEqual((result.width, result.height), (40, 64))
        self.assertEqual(self.rig.encode_calls[-1][0], "text")


class HealthTests(_Case):
    KEYS = {"loaded", "ready", "device", "model_key", "variant", "fp8_compute", "dit_device", "encoder_resident", "last_error", "compute_path", "vram_step", "ram_step", "degraded"}

    def test_health_answers_while_the_lock_is_held(self) -> None:
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
        answer: list[dict[str, Any]] = []
        reader = threading.Thread(target=lambda: answer.append(self.service.health()))
        reader.start()
        reader.join(2)
        self.assertFalse(reader.is_alive())
        self.assertEqual(set(answer[0]), self.KEYS)

    def test_unload_reports_what_it_dropped(self) -> None:
        self.assertFalse(self.service.unload())
        self._run()
        health = self.service.health()
        self.assertEqual((health["ready"], health["variant"], health["fp8_compute"], health["vram_step"], health["ram_step"]), (True, "fp8r", "dequant", "max", "keep"))
        self.assertTrue(self.service.unload())
        self.assertFalse(self.service.unload())
        health = self.service.health()
        self.assertEqual((health["loaded"], health["ready"], health["model_key"], health["dit_device"]), (False, False, None, None))
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))

    def test_a_dropped_handle_reads_as_not_held(self) -> None:
        # The manager may drop an idle handle for another requester; health follows it live.
        self._run()
        self.assertTrue(self.service.health()["loaded"])
        self.rig.manager.unload()
        health = self.service.health()
        self.assertEqual((health["loaded"], health["ready"], health["dit_device"], health["encoder_resident"]), (False, False, None, None))

    def test_health_follows_a_reclaim_park_from_the_ledger(self) -> None:
        # Item 3: the locations are the manager's ledger, not the service's
        # bookkeeping, so a park between runs (another requester's reclaim)
        # reads as RAM at once. The run device is `cpu` here, so the held
        # device label is renamed to tell "on the device" from "in RAM".
        self._run(self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP))
        held = self.service._held
        assert held is not None
        held.device = "gpu-label"
        self.assertEqual((self.service.health()["dit_device"], self.service.health()["encoder_resident"]), ("gpu-label", "gpu-label"))
        # The arbiter's reclaim path (`PoolPolicy.execute` -> the manager).
        offer = Offer(pool=Pool.LARGE, key=held.key, part="dit", action=Action.PARK_TO_HOST, device="cpu", frees_device=0, frees_host=0, reload_cost_s=0.0, cls=OfferClass.A, last_used_at=0.0)
        self.assertTrue(self.rig.manager._execute_offer(offer).ok)
        self.assertIs(self.rig.manager.placements(held.key)["dit"].tier, Tier.HOST)
        health = self.service.health()
        self.assertEqual((health["dit_device"], health["encoder_resident"], health["ready"]), ("cpu", "gpu-label", True))

    def test_a_placement_is_read_as_device_ram_or_not_held(self) -> None:
        def placement(tier: Tier | None, blocks_device: int = 0, exec_device: ExecDevice | None = ExecDevice.RUN_DEVICE) -> ComponentPlacement:
            return ComponentPlacement("dit", tier, exec_device, blocks_device, 0, 0, 0, 0)

        cases = (
            (None, (Tier.DROPPED, None)),
            (placement(None), (Tier.DROPPED, None)),
            (placement(Tier.DEVICE), (Tier.DEVICE, ExecDevice.RUN_DEVICE)),
            # Parked in RAM with no block on the card: RAM; juggled with blocks on the card: the device.
            (placement(Tier.HOST), (Tier.HOST, ExecDevice.RUN_DEVICE)),
            (placement(Tier.HOST, blocks_device=2), (Tier.DEVICE, ExecDevice.RUN_DEVICE)),
            # Streamed from the file: what is held is on the card.
            (placement(Tier.DISK), (Tier.DEVICE, ExecDevice.RUN_DEVICE)),
            (placement(Tier.HOST, exec_device=ExecDevice.CPU), (Tier.HOST, ExecDevice.CPU)),
        )
        for given, expected in cases:
            with self.subTest(given=given):
                self.assertEqual(service._placement_of(given), expected)

    def test_a_residency_key_change_makes_a_second_handle(self) -> None:
        self._run()
        first_key = self.service.health()["model_key"]
        self._run(self._options(ae_dtype=AeDtype.BFLOAT16))
        self.assertNotEqual(self.service.health()["model_key"], first_key)
        self.assertEqual(len(self.rig.dits), 2)
        # The first handle stays idle (the arbiter may reclaim it); unload drops both.
        self.assertEqual(self.rig.large_holdings(), (2, 0, 0))
        self.assertTrue(self.service.unload())
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))

    def test_the_model_key_is_computed_from_options_alone(self) -> None:
        options = Flux3Options(model_root=Path("/nowhere/flux3"), variant=WeightFormat.NVFP4)
        self.assertEqual(service._model_key(options, "cuda:0"), "flux3_klein:nvfp4|cuda:0|auto|float32|text|/nowhere/flux3")


class WireIntegrationTests(_Case):
    """The step callback, «Энкодер на CPU», cancellation, the profiles adapter and `status`."""

    def _params(self, **overrides: Any) -> dict[str, Any]:
        return {"source_root": str(self.rig.root), "prompt": "a cat", **overrides}

    def test_the_step_callback_sees_every_denoise_step(self) -> None:
        seen: list[tuple[int, int]] = []
        self.service.generate(Flux3Request(caption="a cat", steps=3), self._options(), step_callback=lambda step, total: seen.append((step, total)))
        self.assertEqual(seen, [(1, 3), (2, 3), (3, 3)])

    def test_encoder_on_cpu_runs_the_encoder_on_the_cpu_at_every_vram_step(self) -> None:
        for vram in (VramStep.MAX, VramStep.PHASE, VramStep.HALF, VramStep.MIN):
            with self.subTest(vram=vram.value):
                result = self._run(self._options(vram_step=vram, ram_step=RamStep.KEEP, encoder_on_cpu=True))
                session = self._session()
                self.assertTrue(session.key.endswith("|host_pin=encoder"), session.key)
                for state in (*session.plan.phases, session.plan.idle):
                    self.assertIs(state.components["encoder"].exec_device, ExecDevice.CPU)
                    self.assertIsNot(state.components["encoder"].tier, Tier.DEVICE)
                self.assertEqual(result.applied.steps_effective, Steps(vram, RamStep.KEEP))
                self.assertEqual(str(self.rig.encode_calls[-1][1]), "cpu")
                self.assertEqual(self.service.health()["encoder_resident"], "cpu")

    def test_a_cancel_ends_the_run_and_unloads_nothing(self) -> None:
        self._run(self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP))
        warm = self.rig.dits[-1]

        def cancel_at(target: Phase) -> Any:
            def observer(phase: Phase, event: str, info: Any) -> None:
                if phase is target and event == "begin":
                    raise service.Flux3Canceled("canceled")

            return observer

        for target in (Phase.DIT_LOAD, Phase.ENCODE, Phase.DECODE):
            with self.subTest(phase=target.value):
                with self.assertRaises(service.Flux3Canceled):
                    self.service.generate(Flux3Request(caption="a cat"), self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP), observer=cancel_at(target))
                self.assertIsNone(self.service.health()["last_error"])
                self.assertEqual(self.rig.large_holdings()[1:], (0, 0), "no session left in use")
        def cancel_after_two(step: int, total: int) -> None:
            if step == 2:
                raise service.Flux3Canceled("mid-denoise")

        before = warm.calls
        with self.assertRaises(service.Flux3Canceled):
            self.service.generate(Flux3Request(caption="a cat", steps=3), self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP), step_callback=cancel_after_two)
        self.assertEqual(warm.calls - before, 2, "the denoise stopped at the step the callback canceled")
        result = self._run(self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP))
        self.assertIs(self.rig.dits[-1], warm, "the warm DiT survived every cancel")
        self.assertFalse(result.applied.degraded)

    def test_the_profiles_adapter(self) -> None:
        for mode in ("text", "vl_refs"):
            for ae in ("float32", "bfloat16"):
                with self.subTest(mode=mode, ae=ae):
                    self.assertEqual(self.service.large_model_name(self._params(encoder_mode=mode, ae_dtype=ae)), f"flux3_klein.{mode}.ae_{ae}")
        options, device = self.service.large_model_inputs(self._params(format="nvfp4"))
        self.assertEqual((device, options.device, options.options.variant), ("cpu", "cpu", WeightFormat.NVFP4))
        self.assertTrue(options.options.encoder_on_cpu, "absent = «Энкодер на CPU» on")
        with self.assertRaises(ValueError):
            self.service.large_model_inputs(self._params(bogus=1))
        report = self.manager_profiles(self._params())
        self.assertEqual(report.format, "fp8r")
        self.assertIsNotNone(report.selected)

    def manager_profiles(self, params: dict[str, Any]) -> Any:
        from ...runtime.large_models.spec import RequestShape

        options, device = self.service.large_model_inputs(params)
        return self.rig.manager.profiles(self.service.large_model_name(params), options, RequestShape(64, 64, references=((64, 64),)), device=device)

    def test_status_and_an_explicit_dequant_never_probe(self) -> None:
        calls: list[Any] = []
        with patch.object(quant, "probe_scaled_mm", lambda device: calls.append(device) or quant.ScaledMmProbe(supported=False, reason="test", rel_error=None)):
            self.service.status(self._params(format="nvfp4"))
            self.service.large_model_inputs(self._params(format="nvfp4", fp8_compute="dequant"))
        self.assertEqual(calls, [])

    def test_status(self) -> None:
        answer = self.service.status(self._params())
        self.assertTrue(answer["available"])
        self.assertIsNone(answer["reason"])
        self.assertEqual(set(answer["formats"]), {"bf16", "fp8r", "nvfp4"})
        self.assertTrue(answer["formats"]["fp8r"]["available"])
        self.assertFalse(answer["formats"]["bf16"]["available"])
        self.assertGreater(answer["formats"]["fp8r"]["dit_bytes"], 0)
        self.assertTrue(answer["ae_present"] and answer["text_encoder_present"])
        self.assertIn("ready", answer)
        self.assertFalse(self.service.status(self._params(format="bf16"))["available"])
        empty = self.service.status({})
        self.assertFalse(empty["available"])
        self.assertTrue(empty["reason"])


if __name__ == "__main__":
    unittest.main()
