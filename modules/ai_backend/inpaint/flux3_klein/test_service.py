"""
File: modules/ai_backend/inpaint/flux3_klein/test_service.py

Purpose:
Unit tests of `Flux3KleinService` over the real `LargeModelManager`
(`_service_test_fixtures.ServiceRig`): the two handles (pipeline, text
encoder) admitted under the service lock in that order, the encoder under
its combined VRAM-axis ceiling, a refused encoder that finishes the pipeline
with nothing read, the lock order against a concurrent reclaim
(`LockOrderTests`), the two resource steps decide where
the encoder runs, the weight format and the steps are passed to the manager and
never changed, a memory failure degrades the run instead of failing it, a refusal
before / at the admission touches nothing, a failed load registers nothing, a
failed run keeps the handle and releases what its frames held, the reference
rules (VL images = AE images, the reference cap, the crop), `health()` while
the lock is held and read from the manager's ledger (a reclaim park seen live),
`unload`, a residency-key change, the two key formats, a failed load that
drops only the run's pipeline; the step callback, «Энкодер
на CPU» (the encoder on the CPU at every VRAM step), a cancel that unloads
nothing, the `large_models.profiles` adapter (the two-handle composition,
FLUX.2's answer shape) and `status` (per offered DiT and
encoder format; without `source_root` only the root is excused, every other
malformed field is still refused); a GGUF DiT run end to end (compute path `gguf`, the key keyed
by the file, the header check before the admission) and GGUF refusals before
the admission (D6-D9).

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
from . import loading, packing, quant, service, text_encoder
from ._service_test_fixtures import Recorder, ServiceRig
from .config import EncoderSource, GgufDit, WeightFormat
from .params import AeDtype, EncoderMode, Flux3Options, Flux3Request, Fp8Compute
from .phases import Phase


def large_model_shape(caption: str) -> Any:
    """The `RequestShape` a default run of `caption` (no references, the tiny package's 64x64 default size, 4 steps) admits."""
    from .large_model import request_shape

    return request_shape(64, 64, [], caption=caption, steps=4)


class _Case(unittest.TestCase):
    #: The native exports the rig's package holds.
    VARIANTS: tuple[WeightFormat, ...] = (WeightFormat.FP8R, WeightFormat.NVFP4)

    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=self.VARIANTS)
        self.service = self.rig.service
        self.recorder = Recorder()
        # Every admission the service makes: (model, device_cap_bytes, session).
        self.admissions: list[tuple[str, int | None, Any]] = []
        real_acquire = self.rig.manager.acquire

        def acquire(model: str, *args: Any, **kwargs: Any) -> Any:
            session = real_acquire(model, *args, **kwargs)
            self.admissions.append((model, kwargs.get("device_cap_bytes"), session))
            return session

        patcher = patch.object(self.rig.manager, "acquire", acquire)
        patcher.start()
        self.addCleanup(patcher.stop)

    def _options(self, **overrides: Any) -> Flux3Options:
        return Flux3Options(model_root=self.rig.root, variant=overrides.pop("variant", WeightFormat.FP8R), **overrides)

    def _run(self, options: Flux3Options | None = None, request: Flux3Request | None = None) -> service.Flux3Result:
        self.recorder.events.clear()
        return self.service.generate(request or Flux3Request(caption="a cat"), options or self._options(), observer=self.recorder)

    def _session(self) -> Any:
        """The last run's pipeline session."""
        held = self.service._held
        assert held is not None
        return held.session

    def _encoder_session(self) -> Any:
        """The last encoder session the service admitted."""
        return next(session for model, _cap, session in reversed(self.admissions) if model.startswith("flux3_klein_text_encoder."))


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
                plan = self._encoder_session().plan
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
        from ...runtime.large_models import paired

        from .large_model import FLUX3_PAIR

        self._run()
        forecast = self._session().forecast
        encoder = self._encoder_session().forecast
        denoise = self.recorder.begin(Phase.DENOISE)
        self.assertEqual(denoise["forecast_device_peak"], forecast.phases["denoise"].device_peak)
        restore = self.recorder.begin(Phase.DIT_RESTORE)
        self.assertEqual(restore["forecast_device_peak"], forecast.transitions["ref_encode->denoise"].device_peak)
        self.assertEqual(self.recorder.begin(Phase.DIT_LOAD)["forecast_device_peak"], forecast.transitions["idle->ref_encode"].device_peak)
        # The encoder's steps read the encoder handle's forecast; ENCODER_RELEASE (its finish) has none.
        self.assertEqual(self.recorder.begin(Phase.ENCODE)["forecast_device_peak"], encoder.phases["encode"].device_peak)
        self.assertEqual(self.recorder.begin(Phase.ENCODER_LOAD)["forecast_host_peak"], encoder.transitions["idle->encode"].host_peak)
        release = self.recorder.begin(Phase.ENCODER_RELEASE)
        self.assertEqual((release["forecast_device_peak"], release["forecast_host_peak"]), (None, None))
        # The run's peaks are the two-handle rule's, the same in every phase.
        run_device = paired.combined_device_peak(forecast, encoder, FLUX3_PAIR)
        run_host = forecast.host_peak + max(0, encoder.host_peak - paired.host_baseline(encoder))
        for phase in Phase:
            info = self.recorder.begin(phase)
            self.assertEqual((info["forecast_run_device_peak"], info["forecast_run_host_peak"]), (run_device, run_host), phase)

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
        # Both handles stay resident (the pipeline and the encoder).
        self.assertEqual(self.rig.large_holdings(), (2, 0, 0))
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
        self.assertEqual(self._session().model, "flux3_klein.ae_float32")
        self.assertEqual(self._encoder_session().model, "flux3_klein_text_encoder.vl_refs")

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
        offer = Offer(pool=Pool.LARGE, key=held.pipeline_key, part="dit", action=Action.PARK_TO_HOST, device="cpu", frees_device=0, frees_host=0, reload_cost_s=0.0, cls=OfferClass.A, last_used_at=0.0)
        self.assertTrue(self.rig.manager._execute_offer(offer).ok)
        self.assertIs(self.rig.manager.placements(held.pipeline_key)["dit"].tier, Tier.HOST)
        health = self.service.health()
        self.assertEqual((health["dit_device"], health["encoder_resident"], health["ready"]), ("cpu", "gpu-label", True))
        # The encoder is its own handle: dropping it leaves the pipeline ready.
        self.assertTrue(self.rig.manager.unload_key(held.encoder_key))
        health = self.service.health()
        self.assertEqual((health["encoder_resident"], health["ready"], health["loaded"]), (None, True, True))

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
        # The AE dtype keys the pipeline only: one encoder handle serves both
        # pipelines. The first pipeline stays idle (the arbiter may reclaim it);
        # unload drops all three handles.
        self.assertEqual(len(self.rig.encoders), 1)
        self.assertEqual(self.rig.large_holdings(), (3, 0, 0))
        self.assertTrue(self.service.unload())
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))

    def test_the_keys_are_computed_from_options_alone(self) -> None:
        options = Flux3Options(model_root=Path("/nowhere/flux3"), variant=WeightFormat.NVFP4)
        self.assertEqual(service._model_key(options, "cuda:0"), "flux3_klein:nvfp4|cuda:0|auto|float32|/nowhere/flux3")
        self.assertEqual(service._encoder_handle_key(options, "cuda:0"), "flux3_klein_text_encoder:bf16|cuda:0|text|/nowhere/flux3")
        pinned = Flux3Options(model_root=Path("/nowhere/flux3"), variant=WeightFormat.NVFP4, encoder_on_cpu=True)
        self.assertEqual(service._model_key(pinned, "cuda:0"), service._model_key(options, "cuda:0"))
        self.assertEqual(service._encoder_handle_key(pinned, "cuda:0"), "flux3_klein_text_encoder:bf16|cuda:0|text|/nowhere/flux3|host_pin=encoder")

    def test_a_failed_load_unloads_only_the_runs_pipeline(self) -> None:
        # Another pipeline handle (AE bfloat16) and the shared encoder are held
        # idle; a failed load of the float32 pipeline drops its own key only.
        self._run(self._options(ae_dtype=AeDtype.BFLOAT16))
        other = self.service._held
        assert other is not None
        self.rig.fail_ae_build = True
        with self.assertRaises(ValueError):
            self._run()

        def held(key: str) -> bool:
            return any(placement.tier is not None for placement in self.rig.manager.placements(key).values())

        self.assertTrue(held(other.pipeline_key))
        self.assertTrue(held(other.encoder_key))
        self.assertFalse(held(service._model_key(self._options(), "cpu")))
        self.assertEqual(self.rig.large_holdings(), (2, 0, 0))

    def test_a_failed_encoder_read_leaves_the_encoder_handle_holding_nothing(self) -> None:
        # A corrupt shard fails the encoder read part-way (a non-memory error
        # after some tensors were read). The loader discards the partial module
        # and leaves the ledger untouched (`runtime/large_models/loader.py`
        # module notes), so the encoder handle holds nothing and is not
        # forecast as resident; the warm pipeline keeps its between-runs state.
        from . import large_model

        options = self._options(vram_step=VramStep.MAX, ram_step=RamStep.KEEP, encoder_on_cpu=False)
        armed = False
        real = large_model._is_gguf_name

        def observer(phase: Phase, event: str, info: Any) -> None:
            nonlocal armed
            if phase is Phase.ENCODER_LOAD and event == "begin":
                armed = True

        def corrupt(file_key: str) -> bool:
            if armed and file_key.startswith("model.language_model.layers.1"):
                raise OSError("corrupt shard")
            return real(file_key)

        with patch.object(large_model, "_is_gguf_name", corrupt), self.assertRaises(OSError):
            self.service.generate(Flux3Request(caption="a cat"), options, observer=observer)
        encoder = self.rig.manager.placements(service._encoder_handle_key(options, "cpu"))
        self.assertTrue(all(placement.tier is None for placement in encoder.values()), encoder)
        self.assertIsNone(self.service.health()["encoder_resident"])
        self.assertEqual(self.rig.large_holdings(), (1, 0, 0), "the pipeline alone")


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
                session = self._encoder_session()
                self.assertTrue(session.key.endswith("|host_pin=encoder"), session.key)
                self.assertFalse(self._session().key.endswith("|host_pin=encoder"), "the pin is the encoder handle's")
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

    def _shape(self) -> Any:
        from ...runtime.large_models.spec import RequestShape

        return RequestShape(64, 64, references=((64, 64),))

    def test_the_profiles_adapter_composes_both_handles(self) -> None:
        from ...runtime.large_models.tiers import AUTO_STEPS

        for mode in ("text", "vl_refs"):
            for ae in ("float32", "bfloat16"):
                with self.subTest(mode=mode, ae=ae):
                    answer = self.service.large_model_profiles(self._params(encoder_mode=mode, ae_dtype=ae), self._shape(), format=None, steps=AUTO_STEPS)
                    self.assertEqual((answer["model"], answer["format"]), (f"flux3_klein.ae_{ae}", "fp8r"))
                    self.assertTrue(answer["selected"]["fits"])
                    self.assertIn("host_baseline", answer["selected"]["forecast"])
                    self.assertEqual({line["component"] for line in answer["selected"]["plan_summary"]}, {"dit", "ae"})
                    encoder = answer["encoder"]
                    self.assertIsNotNone(encoder, "an uncached caption is encoded: the encoder is counted (served captions: test_prompt_cache)")
                    self.assertEqual({line["component"] for line in encoder["plan_summary"]}, {"encoder"})
                    self.assertIn("host_baseline", encoder["forecast"])
                    self.assertEqual([row["id"] for row in answer["encoder_formats"]], ["bf16"])
        on_cpu = self.service.large_model_profiles(self._params(), self._shape(), format=None, steps=AUTO_STEPS)
        self.assertEqual(on_cpu["encoder"]["plan_summary"][0]["exec_device"], "cpu", "absent encoder_on_cpu = on")
        on_card = self.service.large_model_profiles(self._params(encoder_on_cpu=False), self._shape(), format="nvfp4", steps=AUTO_STEPS)
        self.assertEqual(on_card["encoder"]["plan_summary"][0]["exec_device"], "run_device")
        self.assertEqual(on_card["format"], "nvfp4")
        with self.assertRaises(ValueError):
            self.service.large_model_profiles(self._params(bogus=1), self._shape(), format=None, steps=AUTO_STEPS)

    def test_the_profiles_report_matches_the_run(self) -> None:
        # The run admits the plans the profiles adapter composes (same models,
        # formats, steps and encoder ceiling chain). The observer reports the
        # ADMITTED forecasts (`Session.forecast`, uncalibrated activations left
        # out, `forecast_calibrated` says so), the report's `run_*_peak` the
        # full ones: compared here over the admitted forecasts of the report.
        from ...runtime.large_models import paired

        from .large_model import FLUX3_PAIR

        steps = Steps(VramStep.PHASE, RamStep.KEEP)
        caption = "a cat"
        shape = large_model_shape(caption)
        report = self.service.plan_profiles(self._params(prompt=caption, encoder_on_cpu=False), shape, format=None, steps=steps)
        self.assertEqual((report.alone.model, report.encoder.model), ("flux3_klein.ae_float32", "flux3_klein_text_encoder.text"))
        self._run(self._options(vram_step=steps.vram, ram_step=steps.ram), Flux3Request(caption=caption, steps=4))
        pipeline, encoder = report.alone.selected.forecast, report.encoder.selected.forecast
        info = self.recorder.begin(Phase.DENOISE)
        self.assertEqual(info["forecast_run_device_peak"], paired.combined_device_peak(pipeline, encoder, FLUX3_PAIR))
        self.assertEqual(info["forecast_run_host_peak"], pipeline.host_peak + max(0, encoder.host_peak - paired.host_baseline(encoder)))
        self.assertGreaterEqual(report.run_device_peak(FLUX3_PAIR), info["forecast_run_device_peak"])

    def test_the_encoder_is_admitted_under_its_combined_ceiling(self) -> None:
        # «Максимум скорости» has no higher step: no ceiling. A lower VRAM step
        # gets the combined peak of the nearest fitting higher step minus its
        # own pipeline's ref_encode weights (`paired` module notes).
        for steps, capped in ((Steps(VramStep.MAX, RamStep.KEEP), False), (Steps(VramStep.PHASE, RamStep.KEEP), True), (Steps(VramStep.MIN, RamStep.KEEP), True)):
            with self.subTest(steps=steps.label):
                self.admissions.clear()
                self._run(self._options(vram_step=steps.vram, ram_step=steps.ram))
                models = [model for model, _cap, _session in self.admissions]
                self.assertEqual(models, ["flux3_klein.ae_float32", "flux3_klein_text_encoder.text"], "pipeline before encoder")
                self.assertIsNone(self.admissions[0][1], "the pipeline is never capped")
                self.assertEqual(self.admissions[1][1] is not None, capped)

    def test_a_refused_encoder_finishes_the_pipeline_with_nothing_read(self) -> None:
        real_acquire = self.rig.manager.acquire

        def refuse_encoder(model: str, *args: Any, **kwargs: Any) -> Any:
            if model.startswith("flux3_klein_text_encoder."):
                raise LargeModelRefused(model, kwargs["format"])
            return real_acquire(model, *args, **kwargs)

        with patch.object(self.rig.manager, "acquire", refuse_encoder), self.assertRaises(LargeModelRefused):
            self._run()
        self.assertEqual(self.recorder.events, [], "no phase: the refusal is part of the admission")
        self.assertEqual(self.rig.dits, [], "nothing was built or read")
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0), "the pipeline session ended, its lease released")
        self.assertIn("flux3_klein_text_encoder", self.service.health()["last_error"])
        self._run()
        self.assertIsNone(self.service.health()["last_error"])

    def test_status_and_an_explicit_dequant_never_probe(self) -> None:
        calls: list[Any] = []
        with patch.object(quant, "probe_scaled_mm", lambda device: calls.append(device) or quant.ScaledMmProbe(supported=False, reason="test", rel_error=None)):
            self.service.status(self._params(format="nvfp4"))
            self.service.plan_profiles(self._params(format="nvfp4", fp8_compute="dequant"), self._shape(), format=None, steps=Steps(VramStep.AUTO, RamStep.AUTO))
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

    def test_status_without_a_root_still_refuses_malformed_params(self) -> None:
        # Only the missing root is the unavailable answer; anything else malformed is a request error naming its field.
        for params in (None, {"source_root": ""}, {"format": "bf16"}):
            with self.subTest(params=params):
                self.assertFalse(self.service.status(params)["available"])
        cases = {
            "bogus": {"bogus": 1},
            "format": {"format": 8},
            "ae_dtype": {"ae_dtype": "fp16"},
            "dit_formats": {"dit_formats": []},
            "encoder_mode": {"text_encoder_formats": [{"id": "q8_0", "path": "/m/q8.gguf"}], "text_encoder_format": "q8_0", "encoder_mode": "vl_refs"},
        }
        for field, params in cases.items():
            with self.subTest(field=field):
                with self.assertRaises(ValueError) as caught:
                    self.service.status(params)
                self.assertIn(field, str(caught.exception))


class GgufRunTests(_Case):
    """`status` per offered format, a GGUF DiT through the stubbed service (the header check patched: the stand-in is no FLUX.3 DiT) and the GGUF refusals."""

    # A bf16 export too: a GGUF DiT reads the root `config.json`.
    VARIANTS = (WeightFormat.FP8R, WeightFormat.NVFP4, WeightFormat.BF16)

    def _params(self, **overrides: Any) -> dict[str, Any]:
        return {"source_root": str(self.rig.root), "prompt": "a cat", **overrides}

    def test_status_reports_every_offered_format(self) -> None:
        dit = self.rig.root / "transformer_gguf" / "dit-Q8_0.gguf"
        dit.parent.mkdir()
        dit.write_bytes(b"x" * 11)
        encoder = self.rig.root / "text_encoder_gguf" / "enc-Q8_0.gguf"
        encoder.parent.mkdir()
        encoder.write_bytes(b"x" * 7)
        params = self._params(
            dit_formats=[{"id": "bf16"}, {"id": "fp8r"}, {"id": "q8_0", "path": str(dit)}, {"id": "nvfp4"}, {"id": "q4_k_m", "path": str(self.rig.root / "transformer_gguf" / "absent.gguf")}],
            format="q8_0",
            text_encoder_formats=[{"id": "q8_0", "path": str(encoder)}, {"id": "q4_k_m", "path": str(self.rig.root / "text_encoder_gguf" / "absent.gguf")}],
            text_encoder_format="q4_k_m",
        )
        answer = self.service.status(params)
        self.assertEqual(list(answer["formats"]), ["bf16", "fp8r", "q8_0", "nvfp4", "q4_k_m"])
        self.assertEqual((answer["available"], answer["formats"]["q8_0"]["dit_bytes"]), (True, 11))
        self.assertFalse(answer["formats"]["q4_k_m"]["available"])
        self.assertIn("absent.gguf", answer["formats"]["q4_k_m"]["reason"])
        encoders = answer["text_encoder_formats"]
        self.assertEqual(list(encoders), ["bf16", "q8_0", "q4_k_m"])
        self.assertTrue(encoders["bf16"]["available"] and encoders["bf16"]["bytes"] > 0)
        self.assertEqual((encoders["q8_0"]["available"], encoders["q8_0"]["bytes"], encoders["q8_0"]["reason"]), (True, 7, None))
        self.assertFalse(encoders["q4_k_m"]["available"])
        # `text_encoder_present` follows the SELECTED encoder format.
        self.assertFalse(answer["text_encoder_present"])
        self.assertTrue(self.service.status({**params, "text_encoder_format": "q8_0"})["text_encoder_present"])
        self.assertTrue(self.service.status(self._params())["text_encoder_present"])
        self.assertEqual(set(self.service.status({})["text_encoder_formats"]), {"bf16"})


    def _gguf_dit(self) -> GgufDit:
        from ...runtime._gguf_test_fixtures import Q, make_tensor, write_gguf

        path = self.rig.root / "transformer_gguf" / "stub-Q8_0.gguf"
        path.parent.mkdir(exist_ok=True)
        # The stand-in's key layout (`StubDit`), F32 rows of 32 so a quantized one would be legal too.
        tensors = [make_tensor(name, Q.F32, (4, 4), seed=index) for index, name in enumerate(("txt_in.weight", "single_blocks.0.weight", "single_blocks.1.weight"))]
        write_gguf(path, tensors, architecture="flux")
        return GgufDit("q8_0", path)

    def test_a_gguf_dit_runs_with_the_gguf_compute_path(self) -> None:
        dit = self._gguf_dit()
        checked: list[str] = []

        def require_gguf_dit(spans: Any, package: Any, *, source: Path, format_id: str) -> dict[str, int]:
            checked.append(format_id)
            return {}

        with patch.object(loading, "require_gguf_dit", require_gguf_dit):
            result = self._run(self._options(variant=dit))
        self.assertEqual(checked, ["q8_0"])
        self.assertEqual((result.applied.variant, result.applied.fp8_compute, result.applied.compute_path), (dit, None, "gguf"))
        self.assertEqual(self.recorder.begin(Phase.DENOISE)["variant"], "q8_0")
        self.assertEqual(self.recorder.begin(Phase.DENOISE)["text_encoder_format"], "bf16")
        health = self.service.health()
        self.assertEqual((health["variant"], health["compute_path"]), ("q8_0", "gguf"))
        self.assertTrue(health["model_key"].endswith(f"|dit={dit.path}"), health["model_key"])

    def test_a_broken_gguf_is_refused_before_the_admission_naming_the_field(self) -> None:
        broken = self.rig.root / "transformer_gguf" / "broken.gguf"
        broken.parent.mkdir(exist_ok=True)
        broken.write_bytes(b"GGUF" + b"\0" * 12)
        encoder_file = self.rig.root / "enc.gguf"
        encoder_file.write_bytes(b"GGUF" + b"\0" * 12)
        for options, field in (
            (self._options(variant=GgufDit("q8_0", broken)), "«format»"),
            (self._options(encoder=EncoderSource("q4_k_m", encoder_file)), "«text_encoder_format»"),
            (self._options(encoder=EncoderSource("q4_k_m", self.rig.root / "absent.gguf")), "«text_encoder_format»"),
        ):
            with self.subTest(options=options.variant.value + "/" + options.encoder.id), self.assertRaises((ValueError, FileNotFoundError)) as caught:
                self._run(options)
            self.assertIn(field, str(caught.exception))
        self.assertEqual(self.recorder.events, [])
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))
        self.assertEqual(self.rig.dits, [])

    def test_the_header_refusals_name_the_wire_field(self) -> None:
        def refuse_dit(*args: Any, **kwargs: Any) -> dict[str, int]:
            raise ValueError("FLUX.3 klein DiT: формат «q8_0» (поле «format», x.gguf): нет весов")

        def refuse_encoder(*args: Any, **kwargs: Any) -> None:
            raise ValueError("FLUX.3 klein (текстовый энкодер): формат «q4_k_m» (поле «text_encoder_format», e.gguf): не Qwen3")

        dit = self._gguf_dit()
        encoder = self.rig.root / "enc.gguf"
        encoder.write_bytes(b"x")
        with patch.object(loading, "require_gguf_dit", refuse_dit), self.assertRaises(ValueError) as caught:
            self._run(self._options(variant=dit))
        self.assertIn("format", str(caught.exception))
        with patch.object(text_encoder, "require_gguf_text_encoder", refuse_encoder), self.assertRaises(ValueError) as caught:
            self._run(self._options(encoder=EncoderSource("q4_k_m", encoder)))
        self.assertIn("text_encoder_format", str(caught.exception))
        self.assertEqual(self.rig.large_holdings(), (0, 0, 0))
        self.assertEqual(self.rig.dits, [])

    def test_a_gguf_encoder_cannot_be_asked_for_vl_refs(self) -> None:
        with self.assertRaises(ValueError):
            self._options(encoder=EncoderSource("q4_k_m", self.rig.root / "enc.gguf"), encoder_mode=EncoderMode.VL_REFS)


class _RecordingLock:
    """`service._lock` stand-in: a real `RLock` that records which threads acquired it and whether inside a reclaim."""

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


class LockOrderTests(_Case):
    """The stated exception of the manager's "acquire before the service lock" rule (module notes of `service.py`).

    A run takes this service's lock, then the pipeline's run lock, then the
    encoder's. Three threads start together under a `Barrier`: such a run, a
    `large_models.profiles` composition (read-only: it must never wait for the
    service lock) and a small-pool admission whose hint is larger than the
    host (the run device is `cpu` here, so the idle FLUX.3 components parked
    in RAM are what a reclaim can free), so its reclaim is offered every idle
    FLUX.3 handle (it may only TRY their run locks). None may deadlock (10 s join), and neither the reclaim
    nor the profiles call may reach the service lock.
    """

    JOIN_S = 10.0
    ROUNDS = 3

    def setUp(self) -> None:
        super().setUp()
        from ...runtime.model_manager import SmallModelManager

        self.small = SmallModelManager(arbiter=self.rig.arbiter)
        self.in_reclaim = threading.local()
        self.lock = _RecordingLock(self.in_reclaim)
        self.service._lock = self.lock  # type: ignore[assignment]
        self.reclaimed: list[tuple[int, str]] = []
        real_execute = self.rig.manager._execute_offer
        fixture = self

        def execute(offer: Any) -> Any:
            fixture.reclaimed.append((threading.get_ident(), offer.key))
            fixture.in_reclaim.active = True
            try:
                return real_execute(offer)
            finally:
                fixture.in_reclaim.active = False

        execute_patch = patch.object(self.rig.manager, "_execute_offer", execute)
        execute_patch.start()
        self.addCleanup(execute_patch.stop)

    def _small_admission(self, key: str) -> None:
        from ...runtime.residency.arbiter import ModelMemoryShortError
        from ...runtime.residency.holdings import FootprintHint

        try:
            lease = self.small.begin_model_use(key, hint=FootprintHint("cpu", device_bytes=0, host_bytes=self.rig.probes.host_available * 4))
        except ModelMemoryShortError:
            return
        lease.mark_load_failed()
        lease.release()

    def _prepare(self) -> None:
        """Idle pipeline and encoder handles parked in RAM (kept between runs)."""
        self._run(self._options(vram_step=VramStep.PHASE, ram_step=RamStep.KEEP))

    def _profiles(self) -> None:
        from ...runtime.large_models.spec import RequestShape
        from ...runtime.large_models.tiers import AUTO_STEPS

        params = {"source_root": str(self.rig.root), "prompt": "a cat", "encoder_on_cpu": False}
        self.service.large_model_profiles(params, RequestShape(64, 64), format=None, steps=AUTO_STEPS)

    def test_the_admission_alone_reclaims_idle_flux3_handles(self) -> None:
        self._prepare()
        self._small_admission("small-model")
        keys = {key for _thread, key in self.reclaimed}
        self.assertTrue(keys and all(key.startswith("flux3_klein") for key in keys), keys)
        self.assertEqual(self.lock.acquired_in_reclaim, 0)

    def test_run_profiles_and_reclaim_never_deadlock(self) -> None:
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
                except BaseException as exc:  # noqa: BLE001 - reported below
                    errors.append(exc)

            works = {
                "run": lambda: self.service.generate(Flux3Request(caption=f"a dog {round_index}"), self._options(vram_step=VramStep.PHASE, ram_step=RamStep.KEEP)),
                "profiles": self._profiles,
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
            self.assertNotIn(idents["profiles"], self.lock.acquired_by, "the profiles composition waited for the service lock")
            self.assertEqual(self.lock.acquired_in_reclaim, 0, "a reclaim reached the service lock")
            self.lock.acquired_by.clear()


if __name__ == "__main__":
    unittest.main()
