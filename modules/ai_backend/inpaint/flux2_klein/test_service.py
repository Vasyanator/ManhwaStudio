"""
File: modules/ai_backend/inpaint/flux2_klein/test_service.py

Purpose:
Unit tests for `service.py`: one whole region edit end to end over the fake torch
stack, the warm-up contract, `status`, `unload`, and every refusal path of
`component_action`.

Main responsibilities:
- verify the run order (transformer + VAE first, then the text encoder), the
  dilated latent mask, and the composite handed back to the caller;
- verify the warm-up belongs to the placement and is skipped by a run that
  cache-hits a pipeline whose weights have not moved;
- verify `whole_region` is verified against the mask instead of trusted;
- verify `text_attention_in_mask` reaches the pipeline as ONE `attention_kwargs`
  mask built from the dilated mask (and nothing without it or under
  `whole_region`), that the pipeline's retained copy is dropped before the
  decode and after a failed denoise, that an empty mask is refused before
  anything loads, and that a negative prompt of another length is refused;
- verify `status` merges residency into the disk facts, and omits it with
  `components_busy: true` rather than waiting for the service lock;
- verify every refusal of `component_action`: unknown component or action, an
  action outside the component's current list, a busy service, and each of the
  three memory-guard refusals;
- verify the transformer read publishes the wire's SECOND progress level
  (`file_step`/`file_total`/`file_label`) without moving `step`/`total`.

Notes:
Module attributes are patched on the module that defines them (`pipeline`,
`hardware`, `components`, `attention`), never on the `flux2_klein` package.
"""

from __future__ import annotations

import json
import sys
import types
import unittest
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import (
    attention,
    components,
    hardware,
    pipeline,
    streaming,
)
# Imported at module scope ON PURPOSE: pulling the IPC handler in from inside a
# test would import the backend's own stack while `sys.modules` still holds this
# suite's fake `torch`/`diffusers`, and the half-initialized modules that leaves
# behind break every later test in the file.
from modules.ai_backend.ipc.handlers.flux2_klein import _progress_forwarder
from modules.ai_backend.runtime.model_manager import LoadedModelManager

from ._test_fixtures import (
    _FakeDevice,
    _FakeEmbeds,
    _FakeImageProcessor,
    _FakeLatents,
    _FakeOutOfMemoryError,
    _PatchRecorder,
    _PlacementFixture,
    _ResidencyModule,
    _TempTreeCase,
    _install_fake_torch,
    _make_fake_vae,
    _png_bytes,
)


# ---------------------------------------------------------------------------
# End-to-end request
# ---------------------------------------------------------------------------
class InpaintRequestTests(_TempTreeCase):
    """Drive `inpaint_image_bytes` with a stubbed pipeline."""

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(
            sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn}
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        for module, name, replacement in (
            (pipeline, "patched_module_to", self.recorder),
            (hardware, "_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.region = np.full((128, 128, 3), 90, dtype=np.uint8)
        self.mask = np.zeros((128, 128), dtype=np.uint8)
        self.mask[48:80, 48:80] = 255
        self.decoded = Image.fromarray(np.full((128, 128, 3), 210, dtype=np.uint8), "RGB")

        self.latents = _FakeLatents()
        self.pipe_calls: list[dict[str, object]] = []
        pipe_calls = self.pipe_calls
        latents = self.latents

        class _Pipe(types.SimpleNamespace):
            #: Set by a test to make the denoise raise after diffusers' bookkeeping.
            raise_in_call: BaseException | None = None

            def __call__(self, **kwargs: object) -> object:
                pipe_calls.append(kwargs)
                # What the real `Flux2KleinInpaintPipeline.__call__` does first,
                # and never undoes: the attention mask stays referenced by the
                # pipeline object after the call returns or raises.
                self._attention_kwargs = kwargs.get("attention_kwargs")
                if self.raise_in_call is not None:
                    raise self.raise_in_call
                return types.SimpleNamespace(images=latents)

        self.pipe = _Pipe(
            # Both on the accelerator, as `_apply_placement` would have left them:
            # `_warmup_pipeline_locked` now refuses a component still on the host.
            vae=_make_fake_vae(self.torch.nn.Module, self.decoded, device_type="cuda"),
            transformer=self.torch.nn.Module("transformer", ptr=0x4000, device_type="cuda"),
            # The real pipeline has no text encoder after the two-phase split.
            text_encoder=None,
            image_processor=_FakeImageProcessor(),
        )
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")
        self.builds = 0
        self.encodes = 0
        #: Names of the run's phases in the order they were entered, so the load
        #: ORDER itself is assertable and not just its effects.
        self.order: list[str] = []

        ensure_patch = patch.object(
            self.service,
            "_ensure_pipeline_locked",
            lambda normalized, model_key, report, region_hw, progress_callback=None: (
                self._install(model_key)
            ),
        )
        ensure_patch.start()
        self.addCleanup(ensure_patch.stop)

        # Phase 1 is stubbed the same way: this class is about the request flow,
        # not about the encoder. `PromptEncodingTests` covers phase 1 itself.
        def _embeds(normalized: dict[str, object], _report: object) -> dict[str, object]:
            self.encodes += 1
            self.order.append("encode")
            # The EFFECTIVE scale, exactly as the real phase asks: a distilled
            # checkpoint pins it to 1.0, so no negative embedding is produced.
            negative = (
                _FakeEmbeds("")
                if self.service._effective_guidance_scale(normalized) > 1.0
                else None
            )
            return {"prompt": _FakeEmbeds(str(normalized["prompt"])), "negative": negative}

        embeds_patch = patch.object(self.service, "_prompt_embeds_locked", _embeds)
        embeds_patch.start()
        self.addCleanup(embeds_patch.stop)

        # The guard has its own tests; here it must not gate a fake tree.
        headroom_patch = patch.object(
            self.service, "_require_headroom_locked", lambda *_args, **_kwargs: None
        )
        headroom_patch.start()
        self.addCleanup(headroom_patch.stop)

    def _install(self, model_key: str) -> object:
        """Stand-in for `_ensure_pipeline_locked`, including its cache-hit branch.

        The cache check is reproduced so that `self.builds` counts real rebuilds:
        an invalidated pipeline must show up here as a second build.
        """
        self.order.append("pipeline")
        if self.service._pipe is not None and self.service._active_key == model_key:
            return self.service._pipe
        self.builds += 1
        # `_apply_placement` puts both GPU-resident components on the device, and
        # the stub has to as well: a rebuild after a failed transformer restore
        # would otherwise hand back a pipeline whose transformer is still parked
        # on the host, which `_warmup_pipeline_locked` correctly refuses.
        self.pipe.transformer.to(self.service._device)
        self.pipe.vae.to(self.service._device)
        self.service._pipe = self.pipe
        self.service._active_key = model_key
        return self.pipe

    def _run(self, **overrides: object) -> dict[str, object]:
        return self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(**overrides),
        )

    def test_a_distilled_checkpoint_gets_no_raised_guidance_and_no_negative(self) -> None:
        # `do_classifier_free_guidance` is `guidance_scale > 1 and not
        # is_distilled`, so on the shipped checkpoint a raised slider buys
        # nothing — but sending it down anyway invites diffusers to branch on a
        # value it has already decided to ignore, and the negative embedding it
        # implies costs a full pass over the 16 GB text encoder.
        self._run(placement="full_gpu", guidance_scale=7.0)
        self.assertAlmostEqual(float(self.pipe_calls[-1]["guidance_scale"]), 1.0)
        self.assertIsNone(self.pipe_calls[-1]["negative_prompt_embeds"])

    def test_an_undeclared_checkpoint_still_gets_the_requested_guidance(self) -> None:
        self.declare_distilled(None)
        self._run(placement="full_gpu", guidance_scale=7.0)
        self.assertAlmostEqual(float(self.pipe_calls[-1]["guidance_scale"]), 7.0)
        self.assertIsNotNone(self.pipe_calls[-1]["negative_prompt_embeds"])

    def test_the_ignored_slider_is_announced_once_per_run(self) -> None:
        # Three places read the scale during one request; the user needs the
        # sentence, not three copies of it.
        with self.assertLogs(svc.log, level="INFO") as captured:
            self._run(placement="full_gpu", guidance_scale=7.0)
        ignored = [line for line in captured.output if "is_distilled" in line]
        self.assertEqual(len(ignored), 1, ignored)

    def test_the_result_is_the_region_size_and_untouched_outside_the_mask(self) -> None:
        import io

        result = self._run(placement="full_gpu", color_match=False, mask_feather_px=0)
        self.assertEqual(result["region_size"], [128, 128])
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(out[outside], self.region[outside]))
        self.assertTrue(np.array_equal(out[63, 63], np.asarray(self.decoded)[63, 63]))

    def test_the_pipeline_is_asked_for_latents_and_a_dilated_mask(self) -> None:
        self._run(placement="full_gpu", mask_dilate_px=8, color_match=False)
        call = self.pipe_calls[0]
        self.assertEqual(call["output_type"], "latent")
        dilated = np.asarray(call["mask_image"], dtype=np.uint8)
        # The latent mask is grown; the composite still uses the original one.
        self.assertGreater(int((dilated > 0).sum()), int((self.mask > 0).sum()))
        self.assertEqual(self.latents.detached, 1)

    def test_the_applied_settings_and_recovery_flag_reach_the_caller(self) -> None:
        result = self._run(placement="encoder_cpu", vae_tiling=True, vae_slicing=False)
        self.assertFalse(result["oom_recovered"])
        self.assertEqual(
            result["applied"],
            {
                "unload_transformer_before_vae": True,
                "vae_tiling": True,
                "vae_slicing": False,
                # The shipped default keeps the encoder now: it arrives after the
                # transformer has left host memory, so there is nothing to free.
                "unload_text_encoder_after_encode": False,
                "text_encoder_fp8": False,
            },
        )

    def test_an_oom_in_the_decode_is_reported_as_recovered(self) -> None:
        self.pipe.vae.oom_left = 1
        result = self._run(placement="full_gpu")
        self.assertTrue(result["oom_recovered"])
        self.assertTrue(result["applied"]["unload_transformer_before_vae"])

    def test_a_marks_reference_reaches_the_pipeline_as_one_region_sized_image(self) -> None:
        import io

        reference = np.full((128, 128, 3), 30, dtype=np.uint8)
        reference[10:20, 10:20] = (255, 0, 0)
        guard_calls: list[dict[str, object]] = []
        guard_patch = patch.object(
            self.service,
            "_require_headroom_locked",
            lambda *_args, **kwargs: guard_calls.append(kwargs),
        )
        guard_patch.start()
        self.addCleanup(guard_patch.stop)

        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            reference_bytes=_png_bytes(reference, "RGB"),
            params=self.params(placement="full_gpu", color_match=False, mask_feather_px=0),
        )
        sent = self.pipe_calls[0]["image_reference"]
        # ONE PIL image, not a list: diffusers batches a list from its first element.
        self.assertIsInstance(sent, Image.Image)
        self.assertEqual(sent.size, (128, 128))
        self.assertTrue(np.array_equal(np.asarray(sent, dtype=np.uint8), reference))
        # The region the model edits stays the clean one ...
        self.assertTrue(
            np.array_equal(np.asarray(self.pipe_calls[0]["image"], dtype=np.uint8), self.region)
        )
        # ... and the reference never leaks into the composite outside the mask.
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(out[outside], self.region[outside]))
        self.assertEqual(guard_calls, [{"with_reference": True}])

    def test_without_a_reference_the_pipeline_gets_no_image_reference(self) -> None:
        self._run(placement="full_gpu")
        self.assertNotIn("image_reference", self.pipe_calls[0])

    def _record_attention_masks(self) -> list[dict[str, object]]:
        """Replace the torch mask builder with a recorder; returns its call list.

        Patched on `attention`, its defining module: the fake torch here cannot
        build a real tensor, and the builder's own tests cover the mask itself.
        """
        built: list[dict[str, object]] = []

        def _build(layout: object, *, dtype_name: str, device: object) -> object:
            built.append({"layout": layout, "dtype_name": dtype_name, "device": device})
            return ("attention-mask", len(built))

        builder_patch = patch.object(attention, "build_text_attention_mask", _build)
        builder_patch.start()
        self.addCleanup(builder_patch.stop)
        return built

    def test_without_text_attention_in_mask_the_call_carries_no_attention_kwargs(self) -> None:
        built = self._record_attention_masks()
        result = self._run(placement="full_gpu")
        self.assertNotIn("attention_kwargs", self.pipe_calls[0])
        self.assertEqual(built, [])
        self.assertIs(result["text_attention_in_mask"], False)

    def test_text_attention_in_mask_sends_one_mask_built_from_the_dilated_mask(self) -> None:
        built = self._record_attention_masks()
        with self.assertLogs(svc.log, level="INFO") as captured:
            result = self._run(placement="full_gpu", text_attention_in_mask=True)
        self.assertEqual(
            self.pipe_calls[0]["attention_kwargs"], {"attention_mask": ("attention-mask", 1)}
        )
        self.assertEqual(len(built), 1)
        layout = built[0]["layout"]
        # 128 px region -> 8 x 8 tokens; the 48..80 square dilated by the default
        # 16 px covers pixels 32..95, i.e. token rows/columns 2..5.
        self.assertEqual(
            (layout.text_tokens, layout.image_tokens, layout.reference_tokens), (512, 64, 0)
        )
        self.assertEqual(layout.inside_tokens, 16)
        self.assertTrue(layout.inside.reshape(8, 8)[2:6, 2:6].all())
        self.assertEqual(built[0]["dtype_name"], "bfloat16")
        self.assertEqual(built[0]["device"], self.service._device)
        self.assertIs(result["text_attention_in_mask"], True)
        # Not in `applied`: that dict is persisted into the user's settings.
        self.assertNotIn("text_attention_in_mask", result["applied"])
        line = next(line for line in captured.output if "text_attention_in_mask=True" in line)
        for fragment in ("L=512", "N=64", "R=0", "S=640", "inside_tokens=16", "mask_bytes="):
            self.assertIn(fragment, line)

    def test_text_attention_in_mask_counts_the_reference_tokens(self) -> None:
        built = self._record_attention_masks()
        self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            reference_bytes=_png_bytes(np.zeros((128, 128, 3), dtype=np.uint8), "RGB"),
            params=self.params(placement="full_gpu", text_attention_in_mask=True),
        )
        layout = built[0]["layout"]
        self.assertEqual(layout.reference_tokens, 64)
        self.assertEqual(layout.sequence_length, 512 + 3 * 64)
        # A region-sized reference is spatially aligned with the region, so its
        # tokens follow the same inside/outside grid.
        self.assertIs(layout.reference_inside, layout.inside)

    def test_text_attention_in_mask_is_a_noop_under_whole_region(self) -> None:
        built = self._record_attention_masks()
        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(np.full((128, 128), 255, dtype=np.uint8), "L"),
            params=self.params(
                placement="full_gpu", whole_region=True, text_attention_in_mask=True
            ),
        )
        self.assertNotIn("attention_kwargs", self.pipe_calls[0])
        self.assertEqual(built, [])
        self.assertIs(result["text_attention_in_mask"], False)

    def test_a_negative_prompt_of_another_length_is_refused(self) -> None:
        # The same mask goes to the conditional and the unconditional pass, so
        # it can only fit both when their text lengths agree.
        built = self._record_attention_masks()
        embeds_patch = patch.object(
            self.service,
            "_prompt_embeds_locked",
            lambda _normalized, _report: {
                "prompt": _FakeEmbeds("edit", text_tokens=512),
                "negative": _FakeEmbeds("", text_tokens=256),
            },
        )
        embeds_patch.start()
        self.addCleanup(embeds_patch.stop)
        with self.assertRaises(ValueError) as caught:
            self._run(placement="full_gpu", text_attention_in_mask=True)
        self.assertIn("256", str(caught.exception))
        self.assertEqual(self.pipe_calls, [])
        self.assertEqual(built, [])

    def test_a_negative_prompt_of_the_same_length_gets_the_same_mask(self) -> None:
        self.declare_distilled(None)
        built = self._record_attention_masks()
        self._run(placement="full_gpu", guidance_scale=7.0, text_attention_in_mask=True)
        self.assertIsNotNone(self.pipe_calls[0]["negative_prompt_embeds"])
        self.assertEqual(len(built), 1)
        self.assertIn("attention_kwargs", self.pipe_calls[0])

    def test_the_pipeline_drops_the_attention_mask_before_the_decode(self) -> None:
        # diffusers keeps `pipe._attention_kwargs` after the call; the service
        # must clear it, or the S x S mask stays on the device through the VAE
        # decode and for as long as the pipeline stays resident.
        self._record_attention_masks()
        seen_at_decode: list[object] = []
        decode = self.service._decode_locked

        def _decode(pipe: object, latents_cpu: object, normalized: object) -> object:
            seen_at_decode.append(pipe._attention_kwargs)
            return decode(pipe, latents_cpu, normalized)

        decode_patch = patch.object(self.service, "_decode_locked", _decode)
        decode_patch.start()
        self.addCleanup(decode_patch.stop)
        self._run(placement="full_gpu", text_attention_in_mask=True)
        self.assertIn("attention_kwargs", self.pipe_calls[0])
        self.assertEqual(seen_at_decode, [None])
        self.assertIsNone(self.pipe._attention_kwargs)

    def test_a_failed_denoise_still_drops_the_attention_mask(self) -> None:
        self._record_attention_masks()
        self.pipe.raise_in_call = RuntimeError("denoise failed")
        with self.assertRaises(RuntimeError):
            self._run(placement="full_gpu", text_attention_in_mask=True)
        self.assertIn("attention_kwargs", self.pipe_calls[0])
        self.assertIsNone(self.pipe._attention_kwargs)

    def test_an_empty_mask_is_refused_before_anything_loads(self) -> None:
        # Without this early check the planner would refuse it only after the
        # transformer load and the prompt encode.
        built = self._record_attention_masks()
        self.mask = np.zeros((128, 128), dtype=np.uint8)
        with (
            self.assertLogs(attention.log, level="WARNING") as captured,
            self.assertRaises(ValueError) as caught,
        ):
            self._run(placement="full_gpu", text_attention_in_mask=True)
        self.assertIn("нет ни одного токена", str(caught.exception))
        self.assertTrue(any("mask_shape=(128, 128)" in line for line in captured.output))
        self.assertEqual(self.order, [])
        self.assertEqual(self.encodes, 0)
        self.assertEqual(self.pipe_calls, [])
        self.assertEqual(built, [])

    def test_an_empty_mask_without_the_flag_is_not_refused_by_it(self) -> None:
        self.mask = np.zeros((128, 128), dtype=np.uint8)
        self._run(placement="full_gpu")
        self.assertEqual(len(self.pipe_calls), 1)

    def test_a_reference_of_another_size_is_refused_before_anything_loads(self) -> None:
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"),
                _png_bytes(self.mask, "L"),
                reference_bytes=_png_bytes(np.zeros((64, 128, 3), dtype=np.uint8), "RGB"),
                params=self.params(),
            )
        self.assertEqual(self.builds, 0)
        self.assertEqual(self.pipe_calls, [])

    def test_a_mask_of_the_wrong_size_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"),
                _png_bytes(np.zeros((64, 64), dtype=np.uint8), "L"),
                params=self.params(),
            )

    def test_a_region_the_pipeline_would_resize_is_refused(self) -> None:
        odd = np.full((120, 128, 3), 90, dtype=np.uint8)
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(odd, "RGB"),
                _png_bytes(np.zeros((120, 128), dtype=np.uint8), "L"),
                params=self.params(),
            )

    def test_the_pipeline_is_built_before_the_text_encoder_is_read(self) -> None:
        # The load ORDER is the memory contract: the 16 GB encoder may only be
        # read once the transformer's host copy is gone, i.e. after the pipeline
        # has been built AND placed.
        self._run(placement="encoder_cpu")
        self.assertEqual(self.order, ["pipeline", "encode"])

    def test_the_order_holds_on_a_cache_hit_too(self) -> None:
        self._run(placement="encoder_cpu", prompt="a cat")
        self.order.clear()
        self._run(placement="encoder_cpu", prompt="a dog")
        self.assertEqual(self.order, ["pipeline", "encode"])

    def _frames(self, **overrides: object) -> list[tuple[str, int, int, str]]:
        """One request, returning every progress frame it emitted."""
        frames: list[tuple[str, int, int, str]] = []
        self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(**overrides),
            progress_callback=lambda *frame: frames.append(frame),
        )
        return frames

    @staticmethod
    def _warmup_frames(
        frames: list[tuple[str, int, int, str]],
    ) -> list[tuple[str, int, int, str]]:
        return [frame for frame in frames if frame[3] == "Прогрев модели"]

    def test_the_building_request_warms_up_and_that_is_not_a_generation_step(self) -> None:
        frames = self._frames(placement="encoder_cpu")
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        warmups = self._warmup_frames(frames)
        self.assertEqual(len(warmups), 1)
        self.assertEqual(warmups[0][0], "load")
        self.assertEqual(warmups[0][1], svc.LOAD_STEP_WARMUP)
        # The generate counter must not have grown by it: `total` there is the
        # number of diffusion steps and nothing else, and no generate frame
        # belongs to the warm-up.
        generate = [frame for frame in frames if frame[0] == "generate"]
        self.assertTrue(generate)
        steps = svc.effective_steps(4, 1.0)
        self.assertTrue(all(total == steps for _p, _s, total, _l in generate))
        self.assertTrue(all(label == "Генерация" for _p, _s, _t, label in generate))

    def test_the_first_progress_frame_arrives_before_the_memory_guard(self) -> None:
        # The encoder check and the memory guard both touch the filesystem, and
        # on a cold page cache that is seconds during which a user who has just
        # pressed «Обработать» sees no progress bar at all. The «preparation»
        # frame therefore opens the scope it names.
        frames: list[tuple[str, int, int, str]] = []
        seen_at_guard: list[list[tuple[str, int, int, str]]] = []
        guard_patch = patch.object(
            self.service,
            "_require_headroom_locked",
            lambda *_args, **_kwargs: seen_at_guard.append(list(frames)),
        )
        guard_patch.start()
        self.addCleanup(guard_patch.stop)

        self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(placement="full_gpu"),
            progress_callback=lambda *frame: frames.append(frame),
        )
        self.assertEqual(
            seen_at_guard[0],
            [("load", svc.LOAD_STEP_PREPARE, svc.LOAD_PHASE_STEPS, "Подготовка запуска FLUX.2 klein")],
        )
        # And the accounting is unchanged: one scale, non-decreasing steps.
        load = [frame for frame in frames if frame[0] == "load"]
        self.assertTrue(all(total == svc.LOAD_PHASE_STEPS for _p, _s, total, _l in load))
        steps = [step for _p, step, _t, _l in load]
        self.assertEqual(steps, sorted(steps))

    def test_a_second_request_on_a_hot_pipeline_does_not_warm_up_again(self) -> None:
        # `full_gpu` leaves the transformer on the card for the decode, so nothing
        # moves between the two runs: the warm-up proved where the weights are and
        # that proof is still valid. Re-running it is what made every hot
        # generation announce «Прогрев модели» before it started.
        self._frames(placement="full_gpu", prompt="a cat")
        frames = self._frames(placement="full_gpu", prompt="a dog")
        self.assertEqual(self.builds, 1)
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        self.assertEqual(self._warmup_frames(frames), [])

    def test_parking_the_transformer_makes_the_next_request_warm_up_again(self) -> None:
        # `encoder_cpu` turns `unload_transformer_before_vae` on by default, so
        # every run moves the transformer off the card and back. That is a real
        # weight move, so the next run owes a real warm-up.
        self._run(placement="encoder_cpu")
        frames = self._frames(placement="encoder_cpu")
        self.assertEqual(self.builds, 1)
        self.assertEqual(self.pipe.vae.warmup_calls, 2)
        self.assertEqual(len(self._warmup_frames(frames)), 1)

    def test_an_unload_makes_the_next_request_warm_up_again(self) -> None:
        self._run(placement="full_gpu")
        self.assertTrue(self.service.unload())
        frames = self._frames(placement="full_gpu")
        self.assertEqual(self.builds, 2)
        self.assertEqual(self.pipe.vae.warmup_calls, 2)
        self.assertEqual(len(self._warmup_frames(frames)), 1)

    def test_an_invalidated_pipeline_warms_up_again(self) -> None:
        # A restore that fails invalidates the pipeline SILENTLY (the run itself
        # succeeded), so the flag has to be cleared there too or the rebuilt
        # pipeline would be generated on without a materialization check.
        failures = [1]

        def explode(pipe: object, device: object) -> None:
            if failures:
                failures.pop()
                raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 18.00 GiB")
            pipe.transformer.to(device)

        restore_patch = patch.object(pipeline, "_restore_transformer_to_device", explode)
        restore_patch.start()
        self.addCleanup(restore_patch.stop)

        self._run(placement="encoder_cpu")
        frames = self._frames(placement="encoder_cpu")
        self.assertEqual(self.builds, 2)
        self.assertEqual(len(self._warmup_frames(frames)), 1)

    def test_the_warmup_decode_does_not_consume_the_runs_oom(self) -> None:
        # The warm-up is a 64x64 decode; an OOM belongs to the real one.
        self.pipe.vae.oom_left = 1
        result = self._run(placement="full_gpu")
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        self.assertTrue(result["oom_recovered"])

    def test_a_second_request_is_a_cache_hit(self) -> None:
        self._run(placement="full_gpu")
        self._run(placement="full_gpu")
        self.assertEqual(self.builds, 1)

    def test_a_failed_restore_makes_the_next_request_rebuild(self) -> None:
        failures = [1]

        def explode(pipe: object, device: object) -> None:
            """Fail the first restore only, so the second request is the clean one."""
            if failures:
                failures.pop()
                raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 18.00 GiB")
            pipe.transformer.to(device)

        restore_patch = patch.object(pipeline, "_restore_transformer_to_device", explode)
        restore_patch.start()
        self.addCleanup(restore_patch.stop)

        # `encoder_cpu` parks the transformer, so the failing restore is reached.
        result = self._run(placement="encoder_cpu")
        self.assertEqual(result["region_size"], [128, 128])

        self._run(placement="encoder_cpu")
        # The invalidated pipeline is rebuilt rather than reused with its
        # transformer left on the host.
        self.assertEqual(self.builds, 2)

    def test_whole_region_regenerates_everything_with_a_solid_mask(self) -> None:
        import io

        solid = np.full((128, 128), 255, dtype=np.uint8)
        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(solid, "L"),
            params=self.params(placement="full_gpu", whole_region=True, mask_feather_px=0),
        )
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        # Every pixel comes from the generated image, not just a painted blob.
        self.assertTrue(np.array_equal(out, np.asarray(self.decoded)))
        # The mask handed to the pipeline is the solid one, undilated.
        latent_mask = np.asarray(self.pipe_calls[0]["mask_image"], dtype=np.uint8)
        self.assertTrue(np.array_equal(latent_mask, solid))

    def test_whole_region_still_feathers_inwards_from_the_region_border(self) -> None:
        import io

        solid = np.full((128, 128), 255, dtype=np.uint8)
        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(solid, "L"),
            params=self.params(placement="full_gpu", whole_region=True, mask_feather_px=12),
        )
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        generated = np.asarray(self.decoded)
        # The centre is fully the edit; the region border is still almost entirely
        # the original page, because the ramp starts at the border and reaches
        # full strength `mask_feather_px` pixels in. That gradient IS the soft
        # join to the rest of the page this mode relies on, and without the zero
        # ring in `_mask_distance_inside` a solid mask would have no contour at
        # all and the feather would be a no-op.
        self.assertTrue(np.array_equal(out[64, 64], generated[64, 64]))
        original, edited = int(self.region[0, 0, 0]), int(generated[0, 0, 0])
        self.assertLess(abs(int(out[0, 0, 0]) - original), abs(edited - original) // 4)
        self.assertNotEqual(int(out[0, 0, 0]), edited)
        # Monotone: the further in, the more of the edit.
        profile = [int(out[row, 64, 0]) for row in range(0, 13)]
        self.assertEqual(profile, sorted(profile))
        self.assertEqual(profile[-1], edited)

    def test_whole_region_refuses_a_mask_that_is_not_solid(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"),
                _png_bytes(self.mask, "L"),
                params=self.params(whole_region=True),
            )
        self.assertIn("whole_region", str(caught.exception))
        # Refused before anything is loaded: the flag and the data disagree, and
        # that is a request error, not a smaller edit.
        self.assertEqual(self.builds, 0)
        self.assertEqual(self.encodes, 0)

    def test_an_rgb_mask_is_refused(self) -> None:
        rgb_mask = np.zeros((128, 128, 3), dtype=np.uint8)
        rgb_mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgb_mask, "RGB"), params=self.params()
            )
        self.assertIn("RGB", str(caught.exception))

    def test_an_rgba_mask_is_refused(self) -> None:
        rgba_mask = np.zeros((128, 128, 4), dtype=np.uint8)
        rgba_mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgba_mask, "RGBA"), params=self.params()
            )
        self.assertIn("RGBA", str(caught.exception))

    def test_the_mask_mode_is_checked_before_anything_is_loaded(self) -> None:
        rgb_mask = np.zeros((128, 128, 3), dtype=np.uint8)
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgb_mask, "RGB"), params=self.params()
            )
        self.assertEqual(self.builds, 0)

    def _write_dimensions(self, hidden_size: int, joint_attention_dim: int) -> None:
        """Give the fixture's two configs real widths, as a checkout carries them."""
        (self.root / "text_encoder" / "config.json").write_text(
            json.dumps({"architectures": ["Qwen3Model"], "hidden_size": hidden_size}),
            encoding="utf-8",
        )
        (self.root / "transformer" / "config.json").write_text(
            json.dumps(
                {
                    "_class_name": "Flux2Transformer2DModel",
                    "joint_attention_dim": joint_attention_dim,
                }
            ),
            encoding="utf-8",
        )

    def test_a_mismatched_encoder_is_refused_before_any_weight_is_read(self) -> None:
        # A 9B encoder (hidden_size 4096 -> 12288 channels) beside a 4B
        # transformer (joint_attention_dim 7680). Without this guard the run
        # loads both, passes the memory forecast and dies as a bare matmul shape
        # error inside the denoise, ~34 GB into the read.
        self._write_dimensions(4096, 7680)
        with self.assertRaises(ValueError) as caught:
            self._run()
        message = str(caught.exception)
        self.assertIn("7680", message)
        self.assertIn("12288", message)
        # Nothing was built and no prompt was encoded: it fires before both.
        self.assertEqual(self.builds, 0)
        self.assertEqual(self.encodes, 0)
        self.assertEqual(self.order, [])

    def test_a_matched_pair_runs_normally(self) -> None:
        # The 4B pair, which is the case the guard must NOT refuse.
        self._write_dimensions(2560, 7680)
        result = self._run()
        self.assertEqual(result["region_size"], [128, 128])
        self.assertEqual(self.builds, 1)

    def test_a_user_pressed_pipeline_load_is_guarded_too(self) -> None:
        # The other route that reads the 18 GB transformer. Refusing only the
        # generation would still let the button spend the whole read first.
        self._write_dimensions(4096, 7680)
        with self.assertRaises(ValueError):
            self.service.component_action(
                self.params(), component="pipeline", action="load"
            )
        self.assertEqual(self.builds, 0)


# ---------------------------------------------------------------------------
# Memory forecast and status
# ---------------------------------------------------------------------------
class WarmupTests(_TempTreeCase):
    """`_warmup_pipeline_locked`: proof that the weights really left the host.

    It is the hinge of the new load order — the 16 GB text encoder is read
    immediately after it — so "placed" has to become "materialized" here, with a
    named error when it did not.
    """

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        cache_patch = patch.object(hardware, "_clear_torch_cache", lambda: None)
        cache_patch.start()
        self.addCleanup(cache_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")
        self.frames: list[tuple[int, str]] = []

    def _pipe(self, *, transformer_device: str = "cuda", **vae_kwargs: object) -> object:
        module_cls = self.torch.nn.Module
        return types.SimpleNamespace(
            vae=_make_fake_vae(module_cls, object(), device_type="cuda", **vae_kwargs),
            transformer=module_cls("transformer", ptr=0x4000, device_type=transformer_device),
        )

    def _warmup(self, pipe: object, **overrides: object) -> bool:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return self.service._warmup_pipeline_locked(
            pipe, normalized, lambda step, label: self.frames.append((step, label))
        )

    def _warmup_if_needed(self, pipe: object, **overrides: object) -> bool:
        """The guarded form the generation path uses."""
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return self.service._warmup_pipeline_if_needed_locked(
            pipe, normalized, lambda step, label: self.frames.append((step, label))
        )

    def test_it_runs_one_tiny_decode_and_reports_a_load_step(self) -> None:
        pipe = self._pipe()
        self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 1)
        self.assertEqual(pipe.vae.decode_calls, 0)
        self.assertEqual(self.frames, [(svc.LOAD_STEP_WARMUP, "Прогрев модели")])

    def test_the_warmup_latent_matches_the_vae_and_the_contract_size(self) -> None:
        pipe = self._pipe(latent_channels=32)
        decoded: list[object] = []
        original = pipe.vae.decode

        def record(latents: object, return_dict: bool = True) -> object:
            decoded.append(latents)
            return original(latents, return_dict)

        pipe.vae.decode = record
        self._warmup(pipe, placement="full_gpu")
        self.assertEqual(
            decoded[0].shape, (1, 32, svc.WARMUP_LATENT_CELLS, svc.WARMUP_LATENT_CELLS)
        )
        self.assertEqual(decoded[0].kwargs["dtype"], pipe.vae.dtype)

    def test_a_component_left_on_the_host_is_refused_by_name(self) -> None:
        pipe = self._pipe(transformer_device="cpu")
        with self.assertRaises(RuntimeError) as caught:
            self._warmup(pipe, placement="encoder_cpu")
        message = str(caught.exception)
        self.assertIn("transformer", message)
        self.assertIn("cpu", message)
        # Refused BEFORE the decode, so a mis-placed pipeline never runs at all.
        self.assertEqual(pipe.vae.warmup_calls, 0)

    def test_the_offload_placements_are_skipped(self) -> None:
        # There the weights are SUPPOSED to sit in host memory between forwards;
        # a warm-up would drag all 9B onto the card and straight back.
        for placement in ("model_cpu_offload", "sequential_cpu_offload"):
            with self.subTest(placement=placement):
                pipe = self._pipe(transformer_device="cpu")
                self.assertFalse(self._warmup(pipe, placement=placement))
                self.assertEqual(pipe.vae.warmup_calls, 0)
                self.assertEqual(self.frames, [])

    def test_a_vae_without_latent_channels_degrades_to_a_synchronization(self) -> None:
        pipe = self._pipe(latent_channels=None)
        with self.assertLogs(svc.log, level="WARNING") as logs:
            self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 0)
        self.assertIn("latent_channels", "\n".join(logs.output))

    def test_a_pipeline_without_a_vae_is_not_warmed_up(self) -> None:
        # The lease-protocol tests install a stand-in with no components at all.
        self.assertFalse(self._warmup(types.SimpleNamespace(), placement="full_gpu"))
        self.assertEqual(self.frames, [])

    # ---- once per placement, not once per request ----
    def test_the_guarded_form_runs_only_while_the_pipeline_is_cold(self) -> None:
        pipe = self._pipe()
        self.assertTrue(self._warmup_if_needed(pipe, placement="full_gpu"))
        self.assertFalse(self._warmup_if_needed(pipe, placement="full_gpu"))
        self.assertFalse(self._warmup_if_needed(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 1)
        self.assertEqual(self.frames, [(svc.LOAD_STEP_WARMUP, "Прогрев модели")])

    def test_the_unguarded_form_always_runs_because_the_user_pressed_it(self) -> None:
        # The `warmup` component action goes straight to the primitive: a button
        # that decided for itself not to act would be a lie about what happened.
        pipe = self._pipe()
        self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 2)

    def test_a_placement_that_owes_no_warmup_is_still_marked_warm(self) -> None:
        # «Skipped» here means «nothing is owed», not «not done yet», so the
        # guarded form must not re-enter it on every request either.
        for placement in ("model_cpu_offload", "sequential_cpu_offload"):
            with self.subTest(placement=placement):
                self.service._pipeline_warmed = False
                pipe = self._pipe(transformer_device="cpu")
                self.assertFalse(self._warmup(pipe, placement=placement))
                self.assertTrue(self.service._pipeline_warmed)

    def test_a_pipeline_that_failed_its_warmup_stays_cold(self) -> None:
        # The materialization refusal is the whole point of the pass: a run that
        # never proved its placement must not be treated as if it had.
        pipe = self._pipe(transformer_device="cpu")
        with self.assertRaises(RuntimeError):
            self._warmup_if_needed(pipe, placement="encoder_cpu")
        self.assertFalse(self.service._pipeline_warmed)
        # And the next attempt really does try again.
        with self.assertRaises(RuntimeError):
            self._warmup_if_needed(pipe, placement="encoder_cpu")


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
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

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


class UnloadTests(unittest.TestCase):
    def test_unload_drops_the_pipeline_and_reports_it(self) -> None:
        service = svc.Flux2KleinInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux2_klein:a"

        with patch.object(service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(service.unload())

        unloaded.assert_called_once_with("flux2_klein:a")
        self.assertIsNone(service._pipe)
        self.assertIsNone(service._active_key)

    def test_unload_without_a_pipeline_is_a_noop(self) -> None:
        self.assertFalse(svc.Flux2KleinInpaintService(LoadedModelManager()).unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        service = svc.Flux2KleinInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux2_klein:a"

        self.assertFalse(service._unload_key("flux2_klein:b"))
        self.assertIsNotNone(service._pipe)


class ComponentStatusTests(_TempTreeCase):
    """`status` merges residency into the existing per-component entries."""

    def setUp(self) -> None:
        super().setUp()
        for module, name, replacement in (
            (components, "is_torch_available", lambda: True),
            (
                hardware,
                "memory_snapshot",
                lambda *_a, **_k: {
                    "vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0
                },
            ),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_an_idle_service_reports_every_component_as_not_loaded(self) -> None:
        out = self.service.status(self.params())
        self.assertFalse(out["components_busy"])
        for name in svc.ACTIONABLE_COMPONENTS:
            with self.subTest(component=name):
                self.assertEqual(out["components"][name]["residency"], svc.RESIDENCY_NOT_LOADED)
                self.assertEqual(out["components"][name]["actions"], ["load"])
        # The disk facts of the SAME entries are untouched.
        self.assertEqual(
            out["components"]["transformer"]["path"], self.paths["transformer_path"]
        )
        self.assertTrue(out["components"]["transformer"]["exists"])

    def test_the_tokenizer_and_the_scheduler_carry_no_residency(self) -> None:
        out = self.service.status(self.params())
        for name in ("tokenizer", "scheduler"):
            with self.subTest(component=name):
                self.assertNotIn("residency", out["components"][name])
                self.assertNotIn("actions", out["components"][name])

    def test_a_loaded_pipeline_is_reported_per_component(self) -> None:
        self.service._pipe = types.SimpleNamespace(
            transformer=_ResidencyModule("cuda"), vae=_ResidencyModule("cuda")
        )
        self.service._active_key = "flux2_klein:test"
        out = self.service.status(self.params())
        self.assertEqual(out["components"]["transformer"]["residency"], svc.RESIDENCY_GPU)
        self.assertEqual(out["components"]["transformer"]["actions"], ["unload", "to_ram"])
        self.assertEqual(out["components"]["vae"]["actions"], ["unload", "warmup"])
        self.assertTrue(out["loaded"])

    def test_a_resident_encoder_is_reported_even_without_a_pipeline(self) -> None:
        # `loaded` is pipeline-only, so this is the one place a service holding
        # 16 GB of Qwen3 becomes visible.
        self.service._text_encoder = _ResidencyModule("cpu")
        out = self.service.status(self.params())
        self.assertFalse(out["loaded"])
        self.assertEqual(out["components"]["text_encoder"]["residency"], svc.RESIDENCY_RAM)
        self.assertEqual(out["components"]["text_encoder"]["actions"], ["unload"])

    def test_a_half_placed_pipeline_is_reported_as_mixed(self) -> None:
        self.service._pipe = types.SimpleNamespace(
            transformer=_ResidencyModule("cuda", "cpu"), vae=_ResidencyModule("cuda")
        )
        self.service._active_key = "flux2_klein:test"
        out = self.service.status(self.params())
        self.assertEqual(out["components"]["transformer"]["residency"], svc.RESIDENCY_MIXED)
        self.assertEqual(out["components"]["transformer"]["actions"], ["unload"])

    def test_a_busy_service_omits_the_residency_instead_of_waiting(self) -> None:
        # A generation holds the lock for its ENTIRE run, so the probe takes it
        # without waiting. The absent keys mean "not known", never "not loaded".
        import threading

        holding = threading.Event()
        release = threading.Event()

        def hold() -> None:
            with self.service._lock:
                holding.set()
                release.wait(10)

        worker = threading.Thread(target=hold, daemon=True)
        worker.start()
        self.addCleanup(worker.join, 10)
        self.addCleanup(release.set)
        self.assertTrue(holding.wait(10))

        out = self.service.status(self.params())
        self.assertTrue(out["components_busy"])
        for name in svc.ACTIONABLE_COMPONENTS:
            with self.subTest(component=name):
                self.assertNotIn("residency", out["components"][name])
                self.assertNotIn("actions", out["components"][name])
        # Everything that does not need the lock is still answered.
        self.assertTrue(out["available"])
        self.assertEqual(out["components"]["vae"]["path"], self.paths["vae_path"])
        self.assertEqual(out["device"], "cuda:0")


class ComponentActionTests(_TempTreeCase):
    """`component_action` performs the action or says why it cannot."""

    TRANSFORMER_BYTES = 18_157_185_168
    TEXT_ENCODER_BYTES = 16_381_516_808
    VAE_BYTES = 168_120_878
    GIB = 1024**3

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        sizes = {
            self.paths["transformer_path"]: self.TRANSFORMER_BYTES,
            self.paths["text_encoder_path"]: self.TEXT_ENCODER_BYTES,
            self.paths["vae_path"]: self.VAE_BYTES,
        }
        self.memory = {
            "ram_free": int(64 * self.GIB),
            "ram_total": int(64 * self.GIB),
            "vram_free": int(31 * self.GIB),
            "vram_total": int(32 * self.GIB),
        }
        for module, name, replacement in (
            (pipeline, "patched_module_to", self.recorder),
            (hardware, "_clear_torch_cache", lambda: None),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
            (components, "_weight_bytes", lambda path: sizes.get(path, 0)),
            # The forecast sizes the encoder through the RESIDENT helper, so it
            # has to be patched too or the encoder drops out of every guard
            # assertion below (see `test_memory.py` for the same pair).
            (
                components,
                "text_encoder_resident_bytes",
                lambda path, *_a, **_k: sizes.get(path, 0),
            ),
            (hardware, "memory_snapshot", lambda *_a, **_k: dict(self.memory)),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.manager = LoadedModelManager()
        self.service = svc.Flux2KleinInpaintService(self.manager)
        self.service._device = _FakeDevice("cuda:0")
        self.decoded = Image.fromarray(np.full((64, 64, 3), 200, dtype=np.uint8), "RGB")
        self.pipe = types.SimpleNamespace(
            vae=_make_fake_vae(self.torch.nn.Module, self.decoded, device_type="cuda"),
            transformer=self.torch.nn.Module("transformer", ptr=0x4000, device_type="cuda"),
            text_encoder=None,
            image_processor=_FakeImageProcessor(),
        )
        self.builds = 0

        def _ensure(normalized, model_key, report, *, region_hw, progress_callback=None):
            self.builds += 1
            self.region_hw = region_hw
            self.pipe.transformer.to(self.service._device)
            self.pipe.vae.to(self.service._device)
            self.service._pipe = self.pipe
            self.service._active_key = model_key
            return self.pipe

        ensure_patch = patch.object(self.service, "_ensure_pipeline_locked", _ensure)
        ensure_patch.start()
        self.addCleanup(ensure_patch.stop)

    def _install(self, **overrides: object) -> str:
        """Put the fake pipeline in place under its real model key."""
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        key = svc._model_key(normalized)
        self.service._pipe = self.pipe
        self.service._active_key = key
        return key

    def _act(self, component: str, action: str, **overrides: object) -> dict[str, object]:
        return self.service.component_action(
            self.params(**overrides), component=component, action=action
        )

    # ---- vocabulary ----
    def test_an_unknown_component_is_a_request_error(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self._act("tokenizer", "load")
        self.assertIn("неизвестный компонент", str(caught.exception))

    def test_an_unknown_action_is_a_request_error(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self._act("vae", "explode")
        self.assertIn("неизвестное действие", str(caught.exception))

    def test_an_action_outside_the_current_list_is_refused_naming_what_is_possible(self) -> None:
        # Nothing is loaded, so «warmup» cannot be performed on the VAE.
        with self.assertRaises(ValueError) as caught:
            self._act("vae", "warmup")
        message = str(caught.exception)
        self.assertIn("«warmup»", message)
        self.assertIn("«load»", message)
        self.assertEqual(self.builds, 0)

    def test_the_transformer_is_never_warmed_up_through_this_method(self) -> None:
        self._install()
        with self.assertRaises(ValueError):
            self._act("transformer", "warmup")

    def test_a_refused_load_leaves_no_entry_stuck_in_loading(self) -> None:
        # The pipeline is already loaded, so «load» is not in the transformer's
        # current actions — but the lease was taken BEFORE the check. Releasing
        # it without resolving it would flag the key `loading` for good and make
        # the next request for it wait forever.
        self._install()
        with self.assertRaises(ValueError):
            self._act("transformer", "load")
        self.assertEqual(self.manager.health()["loading_model_count"], 0)
        # And the very next legitimate action still goes through.
        self.assertTrue(self._act("vae", "unload")["performed"])

    def test_an_unleased_action_never_touches_the_model_manager(self) -> None:
        # `to_gpu` and `warmup` act on a pipeline that is already resident and
        # change no key; an eviction cannot race them because `_unload_key` takes
        # the lock this action holds throughout.
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")
        self._act("transformer", "to_gpu", placement="full_gpu")
        self._act("vae", "warmup", placement="full_gpu")
        health = self.manager.health()
        self.assertEqual(health["loading_model_count"], 0)
        self.assertEqual(health["active_model_count"], 0)
        self.assertEqual(health["resident_model_count"], 0)

    def test_the_text_encoder_is_never_moved_to_the_gpu(self) -> None:
        self.service._text_encoder = _ResidencyModule("cpu")
        with self.assertRaises(ValueError):
            self._act("text_encoder", "to_gpu")

    # ---- the moves invalidate the warm-up ----
    def test_parking_the_transformer_marks_the_pipeline_cold(self) -> None:
        self._install(placement="full_gpu")
        self.service._pipeline_warmed = True
        self.assertTrue(self._act("transformer", "to_ram", placement="full_gpu")["performed"])
        self.assertFalse(self.service._pipeline_warmed)

    def test_restoring_the_transformer_marks_the_pipeline_cold(self) -> None:
        # A restore is a queued host->device copy exactly like a placement, so it
        # leaves a warm-up owed rather than satisfied.
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")
        self.service._pipeline_warmed = True
        self.assertTrue(self._act("transformer", "to_gpu", placement="full_gpu")["performed"])
        self.assertFalse(self.service._pipeline_warmed)

    def test_the_warmup_button_runs_on_an_already_warm_pipeline(self) -> None:
        self._install(placement="full_gpu")
        self.service._pipeline_warmed = True
        result = self._act("vae", "warmup", placement="full_gpu")
        self.assertTrue(result["performed"])
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        self.assertTrue(self.service._pipeline_warmed)

    def test_unloading_the_pipeline_marks_it_cold(self) -> None:
        self._install(placement="full_gpu")
        self.service._pipeline_warmed = True
        self._act("vae", "unload", placement="full_gpu")
        self.assertFalse(self.service._pipeline_warmed)

    # ---- busy ----
    def test_a_busy_service_refuses_instead_of_waiting(self) -> None:
        import threading

        holding = threading.Event()
        release = threading.Event()

        def hold() -> None:
            with self.service._lock:
                holding.set()
                release.wait(10)

        worker = threading.Thread(target=hold, daemon=True)
        worker.start()
        self.addCleanup(worker.join, 10)
        self.addCleanup(release.set)
        self.assertTrue(holding.wait(10))

        with self.assertRaises(RuntimeError) as caught:
            self._act("transformer", "load")
        self.assertIn("занят", str(caught.exception))
        self.assertEqual(self.builds, 0)
        # Nothing was left leased behind the refusal — and, crucially, no entry
        # was left flagged `loading`: the next request for that key would wait on
        # a load that never finishes.
        health = self.manager.health()
        self.assertEqual(health["active_model_count"], 0)
        self.assertEqual(health["loading_model_count"], 0)
        self.assertEqual(health["resident_model_count"], 0)

    # ---- the pipeline pair ----
    def test_loading_the_transformer_builds_the_whole_pipeline_and_warms_it_up(self) -> None:
        result = self._act("transformer", "load")
        self.assertEqual(self.builds, 1)
        self.assertTrue(result["performed"])
        self.assertFalse(result["components_busy"])
        self.assertEqual(result["components"]["vae"]["residency"], svc.RESIDENCY_GPU)
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        # It is registered with the model manager exactly as a generation would.
        self.assertEqual(self.manager.health()["resident_model_count"], 1)
        self.assertEqual(self.manager.health()["active_model_count"], 0)

    def test_the_load_is_asked_for_the_smallest_valid_region(self) -> None:
        self._act("vae", "load")
        self.assertEqual(self.region_hw, (svc.MIN_REGION_SIDE, svc.MIN_REGION_SIDE))

    def test_unloading_the_vae_drops_the_pipeline_but_keeps_the_encoder(self) -> None:
        key = self._install()
        self.manager.begin_model_use(key).mark_loaded()
        self.service._text_encoder = _ResidencyModule("cpu")

        result = self._act("vae", "unload")
        self.assertTrue(result["performed"])
        self.assertIsNone(self.service._pipe)
        # The encoder is NOT part of the pipeline and must not be a silent
        # casualty of an unload the user asked for on the VAE.
        self.assertIsNotNone(self.service._text_encoder)
        self.assertEqual(result["components"]["text_encoder"]["residency"], svc.RESIDENCY_RAM)
        self.assertEqual(self.manager.health()["resident_model_count"], 0)

    # ---- moving the transformer ----
    def test_parking_the_transformer_reports_it_in_ram_afterwards(self) -> None:
        self._install(placement="full_gpu")
        result = self._act("transformer", "to_ram", placement="full_gpu")
        self.assertTrue(result["performed"])
        self.assertEqual(result["components"]["transformer"]["residency"], svc.RESIDENCY_RAM)
        self.assertEqual(result["components"]["transformer"]["actions"], ["unload", "to_gpu"])

    def test_restoring_the_transformer_moves_it_inside_the_staging_patch(self) -> None:
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")
        self.pipe.transformer.moves.clear()

        result = self._act("transformer", "to_gpu", placement="full_gpu")
        self.assertTrue(result["performed"])
        self.assertEqual(result["components"]["transformer"]["residency"], svc.RESIDENCY_GPU)
        self.assertEqual([depth for _target, depth in self.pipe.transformer.moves], [1])

    def test_a_failed_restore_invalidates_the_cached_pipeline(self) -> None:
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")

        def _boom(*_args: object, **_kwargs: object) -> None:
            raise _FakeOutOfMemoryError("HIP out of memory")

        with patch.object(pipeline, "_restore_transformer_to_device", _boom):
            with self.assertRaises(_FakeOutOfMemoryError):
                self._act("transformer", "to_gpu", placement="full_gpu")
        # Its transformer is on the host, so the key no longer describes it: the
        # next request must rebuild rather than cache-hit onto a mismatch.
        self.assertIsNone(self.service._pipe)
        self.assertIsNone(self.service._active_key)

    def test_a_failed_restore_marks_the_pipeline_cold(self) -> None:
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")
        # Marked warm again AFTER the park, so the invalidation is the only thing
        # left that can clear it: the park's own clearing cannot carry this case.
        self.service._pipeline_warmed = True

        def _boom(*_args: object, **_kwargs: object) -> None:
            raise _FakeOutOfMemoryError("HIP out of memory")

        with patch.object(pipeline, "_restore_transformer_to_device", _boom):
            with self.assertRaises(_FakeOutOfMemoryError):
                self._act("transformer", "to_gpu", placement="full_gpu")
        self.assertFalse(self.service._pipeline_warmed)

    # ---- the text encoder ----
    def test_loading_the_text_encoder_keeps_it_and_reports_it(self) -> None:
        loaded = _ResidencyModule("cpu")
        with patch.object(
            self.service,
            "_ensure_text_encoder_locked",
            lambda _n, _r, what: (loaded, ("k",), _FakeDevice("cpu")),
        ):
            result = self._act("text_encoder", "load")
        self.assertTrue(result["performed"])
        self.assertIs(self.service._text_encoder, loaded)
        self.assertEqual(result["components"]["text_encoder"]["residency"], svc.RESIDENCY_RAM)
        self.assertEqual(result["components"]["text_encoder"]["actions"], ["unload"])

    def test_unloading_the_text_encoder_leaves_the_pipeline_alone(self) -> None:
        self._install()
        self.service._text_encoder = _ResidencyModule("cpu")
        result = self._act("text_encoder", "unload")
        self.assertTrue(result["performed"])
        self.assertIsNone(self.service._text_encoder)
        self.assertIsNotNone(self.service._pipe)

    # ---- the memory guard ----
    def test_a_pipeline_load_that_does_not_fit_is_refused_before_anything_is_read(self) -> None:
        self.memory["vram_free"] = int(8 * self.GIB)
        with self.assertRaises(RuntimeError) as caught:
            self._act("transformer", "load", placement="encoder_cpu")
        message = str(caught.exception)
        self.assertIn("видеопамяти на cuda:0", message)
        self.assertIn("Загрузка не начата", message)
        self.assertEqual(self.builds, 0)
        # A refused load leaves nothing resident and nothing leased.
        health = self.manager.health()
        self.assertEqual(health["resident_model_count"], 0)
        self.assertEqual(health["active_model_count"], 0)
        self.assertEqual(health["loading_model_count"], 0)

    def test_an_encoder_load_goes_through_the_same_standalone_encode_guard(self) -> None:
        # 16 GiB of Qwen3 on a host with 12 GiB free: the same refusal a
        # `prompt_cache.build` gets, with the same numbers.
        self.memory["ram_free"] = int(12 * self.GIB)
        with self.assertRaises(RuntimeError) as caught:
            self._act("text_encoder", "load")
        message = str(caught.exception)
        self.assertIn("оперативной памяти на этап «кодирование промпта (кэширование)»", message)
        self.assertIsNone(self.service._text_encoder)

    def test_a_restore_that_does_not_fit_is_refused_before_the_copy(self) -> None:
        self._install(placement="full_gpu")
        self._act("transformer", "to_ram", placement="full_gpu")
        self.pipe.transformer.moves.clear()
        self.memory["vram_free"] = int(4 * self.GIB)

        with self.assertRaises(RuntimeError) as caught:
            self._act("transformer", "to_gpu", placement="full_gpu")
        self.assertIn("видеопамяти на cuda:0 на этап «денойз»", str(caught.exception))
        self.assertEqual(self.pipe.transformer.moves, [])
        # The pipeline is untouched: a refusal is not an invalidation.
        self.assertIsNotNone(self.service._pipe)

    # ---- the warm-up ----
    def test_warming_up_the_vae_runs_one_tiny_decode(self) -> None:
        self._install(placement="full_gpu")
        result = self._act("vae", "warmup", placement="full_gpu")
        self.assertTrue(result["performed"])
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        self.assertEqual(self.pipe.vae.decode_calls, 0)

# ---------------------------------------------------------------------------
# Byte-level load progress
# ---------------------------------------------------------------------------
class _RecordingEmitter:
    """The one method `_progress_forwarder` calls on a request's progress emitter."""

    def __init__(self) -> None:
        self.frames: list[dict[str, object]] = []

    def emit(self, header: dict[str, object], blob: bytes) -> None:
        assert blob == b""
        self.frames.append(dict(header))


class TransformerByteProgressTests(_PlacementFixture):
    """The transformer read drives the wire's SECOND progress level.

    The step bar cannot move for the minutes a ~17 GiB checkpoint takes, so the
    streaming loader's byte counters are published as the optional
    `file_step`/`file_total`/`file_label` fields of the SAME frame. The real IPC
    forwarder is used here rather than a stand-in, because "does it reach the
    handler" is precisely the question.
    """

    single_file_transformer = True
    TOTAL_BYTES = 17 * 1024**3

    def setUp(self) -> None:
        super().setUp()
        self.emitter = _RecordingEmitter()
        self.forward = _progress_forwarder(types.SimpleNamespace(progress_emitter=self.emitter))
        module_cls = self.torch.nn.Module
        total = self.TOTAL_BYTES

        def _stream(model_cls: object, _source: object, **kwargs: object) -> object:
            progress = kwargs.get("progress")
            if callable(progress):
                progress(total // 4, total, "blocks.0.weight")
                progress(total, total, "blocks.9.weight")
            return module_cls("transformer", ptr=0x4000, device_type="cuda:0")

        stream_patch = patch.object(streaming, "load_transformer_streaming", _stream)
        stream_patch.start()
        self.addCleanup(stream_patch.stop)

    def _load(self) -> list[dict[str, object]]:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="encoder_cpu", low_cpu_mem_usage=True)
        )
        report = svc._progress_reporter(self.forward, "load", svc.LOAD_PHASE_STEPS)
        self.service._ensure_pipeline_locked(
            normalized,
            "flux2_klein:test",
            report,
            region_hw=(128, 128),
            progress_callback=self.forward,
        )
        return self.emitter.frames

    def test_the_bytes_arrive_as_file_step_and_file_total(self) -> None:
        byte_frames = [frame for frame in self._load() if "file_step" in frame]
        self.assertEqual(
            [(frame["file_step"], frame["file_total"]) for frame in byte_frames],
            [(self.TOTAL_BYTES // 4, self.TOTAL_BYTES), (self.TOTAL_BYTES, self.TOTAL_BYTES)],
        )
        # The FILE is named, not the tensor: the client renders this label beside
        # a GiB figure, and a per-tensor key would flicker on every frame.
        for frame in byte_frames:
            self.assertEqual(frame["file_label"], "flux2-klein.safetensors")

    def test_the_step_level_is_not_disturbed(self) -> None:
        byte_frames = [frame for frame in self._load() if "file_step" in frame]
        for frame in byte_frames:
            with self.subTest(file_step=frame["file_step"]):
                self.assertEqual(frame["phase"], "load")
                self.assertEqual(frame["step"], svc.LOAD_STEP_TRANSFORMER)
                self.assertEqual(frame["total"], svc.LOAD_PHASE_STEPS)
                self.assertEqual(frame["label"], "Загрузка трансформера")

    def test_the_later_step_frames_carry_no_file_level_and_so_clear_the_bar(self) -> None:
        # The Rust client clears its second bar on the first frame without the
        # fields, which is what makes the tokenizer/VAE steps look right.
        frames = self._load()
        later = [
            frame
            for frame in frames
            if isinstance(frame["step"], int) and frame["step"] > svc.LOAD_STEP_TRANSFORMER
        ]
        self.assertTrue(later)
        for frame in later:
            with self.subTest(step=frame["step"]):
                self.assertNotIn("file_step", frame)
                self.assertNotIn("file_total", frame)
                self.assertNotIn("file_label", frame)

    def test_a_callback_that_predates_the_second_level_is_not_broken_by_it(self) -> None:
        # Every service callback older than the download's three fields takes
        # four positional arguments only. Handing it keywords raises `TypeError`,
        # which must cost the byte frames and nothing else.
        seen: list[tuple[str, int]] = []
        report = svc._progress_reporter(
            lambda phase, step, _total, _label: seen.append((phase, step)),
            "load",
            svc.LOAD_PHASE_STEPS,
        )
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="encoder_cpu", low_cpu_mem_usage=True)
        )
        self.service._ensure_pipeline_locked(
            normalized,
            "flux2_klein:test",
            report,
            region_hw=(128, 128),
            progress_callback=lambda phase, step, _total, _label: seen.append((phase, step)),
        )
        self.assertIn(("load", svc.LOAD_STEP_TRANSFORMER), seen)
        self.assertIn(("load", svc.LOAD_STEP_PLACEMENT), seen)


if __name__ == "__main__":
    unittest.main()
