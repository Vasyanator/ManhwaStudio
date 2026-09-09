"""
File: modules/ai_backend/inpaint/flux2_klein/test_pipeline.py

Purpose:
Unit tests for `pipeline.py`: the four placement paths, the transformer loaders
and their config contract, the split denoise/decode with its out-of-memory
recovery, and the per-component residency probe and action matrix.

Main responsibilities:
- verify each placement moves exactly the components it promises, inside the
  ROCm staging patch, and that `low_cpu_mem_usage` loads straight to the device;
- verify a single-file transformer is loaded with the local
  `transformer/config.json` and refused with instructions when there is none, so
  the gated `flux-2-dev` config is never fetched;
- verify the choice between the streaming loader and `from_single_file`: the
  streaming one whenever `streaming.streaming_load_eligible` says so, the
  ordinary one otherwise WITH the refusal logged at the call site;
- verify the transformer is parked off the GPU before the VAE decode and moved
  back afterwards, and that an OOM in the decode is recovered from without
  repeating the denoise;
- verify the residency probe answers each of its five states, that `mixed` is
  never rounded into a neighbour, and that an accelerate hook means `offloaded`;
- verify the per-component `actions` invariants of the wire contract.

Notes:
Module attributes are patched on the module that defines them (`pipeline`,
`hardware`), never on the `flux2_klein` package - see `_test_fixtures.py`.
"""

from __future__ import annotations

import json
import sys
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware, pipeline, streaming
from modules.ai_backend.runtime.model_manager import LoadedModelManager

from ._test_fixtures import (
    _FakeDevice,
    _FakeLatents,
    _FakeOutOfMemoryError,
    _PatchRecorder,
    _PlacementFixture,
    _ResidencyModule,
    _TempTreeCase,
    _fake_pipe,
    _install_fake_torch,
    _make_fake_vae,
    _write_safetensors,
)

#: Checkpoint size the streaming double reports through its byte-progress
#: callback. Any number does; it only has to be stable so a test can assert on it.
STREAM_TOTAL_BYTES = 8 * 1024 * 1024


class PipelinePlacementTests(_PlacementFixture):
    """Cover which placement path `_ensure_pipeline_locked` takes (transformer folder)."""

    def test_full_gpu_moves_the_pipeline_inside_the_staging_patch(self) -> None:
        pipe = self._build(placement="full_gpu")
        self.assertEqual([depth for _dev, depth in pipe.moves], [1])
        self.assertEqual(pipe.offload_devices, [])
        self.assertEqual(self.recorder.depth, 0)
        self.assertNotIn("device_map", self.load_kwargs["transformer"])

    def test_low_cpu_mem_usage_loads_straight_onto_the_device(self) -> None:
        pipe = self._build(placement="full_gpu", low_cpu_mem_usage=True)
        for component in ("transformer", "vae"):
            with self.subTest(component=component):
                self.assertEqual(self.load_kwargs[component]["device_map"], {"": "cuda:0"})
                self.assertEqual(str(getattr(pipe, component).device), "cuda:0")
        # The placement move still happens, and inside the staging patch: a
        # loader that silently ignores its placement kwarg must not be able to
        # leave a component on the host (see SingleFileTransformerPlacementTests).
        self.assertEqual([depth for _dev, depth in pipe.moves], [1])

    def test_the_checkpoints_own_is_distilled_reaches_the_constructor(self) -> None:
        # The shipped klein 4B declares it, and the flag is the difference
        # between one and two transformer passes per denoise step.
        self.assertIs(self._build(placement="full_gpu").is_distilled, True)

    def test_a_checkpoint_declaring_false_builds_a_guided_pipeline(self) -> None:
        self.declare_distilled(False)
        self.assertIs(self._build(placement="full_gpu").is_distilled, False)

    def test_an_undeclared_checkpoint_builds_as_not_distilled_and_says_so(self) -> None:
        # The documented policy for the unknown case: today's behaviour, kept,
        # and logged — never a refusal, because a hand-assembled folder of
        # components has no manifest at all.
        self.declare_distilled(None)
        with self.assertLogs(svc.log, level="INFO") as captured:
            pipe = self._build(placement="full_gpu")
        self.assertIs(pipe.is_distilled, False)
        self.assertTrue(any("is_distilled" in line for line in captured.output))

    def test_a_non_boolean_declaration_is_treated_as_undeclared(self) -> None:
        # A hand-edited manifest saying `"true"` must not read as True, the same
        # rule `_config_int` applies to dimensions.
        self.declare_distilled("true")
        self.assertIs(self._build(placement="full_gpu").is_distilled, False)

    def test_a_rebuild_marks_the_pipeline_cold(self) -> None:
        # A build places weights, so whatever a previous warm-up proved is gone
        # with the pipeline that was dropped.
        self._build(model_key="flux2_klein:a", placement="full_gpu")
        self.service._pipeline_warmed = True
        self._build(model_key="flux2_klein:b", placement="full_gpu")
        self.assertFalse(self.service._pipeline_warmed)

    def test_a_cache_hit_leaves_the_warm_pipeline_warm(self) -> None:
        # Nothing moved, so nothing the warm-up proved stopped being true. This
        # is the whole of the once-per-placement rule on the build side.
        self._build(model_key="flux2_klein:a", placement="full_gpu")
        self.service._pipeline_warmed = True
        self._build(model_key="flux2_klein:a", placement="full_gpu")
        self.assertTrue(self.service._pipeline_warmed)

    def test_the_tokenizer_is_read_once_per_directory(self) -> None:
        # It is a few MB of vocabulary on the same disk as the 18 GB checkpoint,
        # and it used to be re-read on every prompt encode.
        pipe = self._build(model_key="flux2_klein:a", placement="full_gpu")
        self._build(model_key="flux2_klein:b", placement="full_gpu")
        self._encode(placement="full_gpu", prompt="a cat")
        self.assertEqual(self.load_calls["tokenizer"], 1)
        # The pipeline component and the encode path are the same object.
        self.assertIs(pipe.tokenizer, self.service._tokenizer)

    def test_another_tokenizer_directory_is_a_miss(self) -> None:
        first = self.service._ensure_tokenizer_locked(self.root / "tokenizer")
        other = self.root / "tokenizer_two"
        other.mkdir()
        (other / "tokenizer_config.json").write_text("{}", encoding="utf-8")
        second = self.service._ensure_tokenizer_locked(other)
        self.assertEqual(self.load_calls["tokenizer"], 2)
        self.assertIsNot(first, second)
        self.assertIs(second, self.service._tokenizer)

    def test_unload_drops_the_cached_tokenizer(self) -> None:
        self._build(model_key="flux2_klein:a", placement="full_gpu")
        self.service.unload()
        self.assertIsNone(self.service._tokenizer)
        self._build(model_key="flux2_klein:a", placement="full_gpu")
        self.assertEqual(self.load_calls["tokenizer"], 2)

    def test_the_pipeline_is_built_without_a_text_encoder(self) -> None:
        # The whole point of the two-phase run: 8B of Qwen3 must never be
        # resident next to the 9B transformer.
        pipe = self._build(placement="full_gpu")
        self.assertIsNone(pipe.text_encoder)
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_encoder_cpu_moves_only_the_transformer_and_the_vae(self) -> None:
        pipe = self._build(placement="encoder_cpu")
        self.assertEqual(pipe.moves, [])
        self.assertEqual([depth for _dev, depth in pipe.transformer.moves], [1])
        self.assertEqual([depth for _dev, depth in pipe.vae.moves], [1])

    def test_model_cpu_offload_hands_placement_to_accelerate(self) -> None:
        with patch.object(pipeline, "mmap_staging_required", return_value=False):
            pipe = self._build(placement="model_cpu_offload")
        self.assertEqual(pipe.offload_devices, ["cuda:0"])
        self.assertEqual(pipe.moves, [])

    def test_sequential_cpu_offload_hands_placement_to_accelerate(self) -> None:
        with patch.object(pipeline, "mmap_staging_required", return_value=False):
            pipe = self._build(placement="sequential_cpu_offload")
        self.assertEqual(pipe.sequential_offload_devices, ["cuda:0"])

    def test_offload_rehomes_the_file_backed_components_first(self) -> None:
        with patch.object(pipeline, "mmap_staging_required", return_value=True):
            with patch.object(pipeline, "tensor_needs_staging", return_value=True):
                pipe = self._build(placement="model_cpu_offload")
        # One staged move onto the GPU (patch depth 1) plus the move back that
        # allocates anonymous host memory, per component.
        for component in ("vae", "transformer"):
            with self.subTest(component=component):
                moves = getattr(pipe, component).moves
                self.assertEqual([depth for _dev, depth in moves], [1, 0])
        self.assertEqual(self.recorder.depth, 0)

    def test_the_transformer_is_rehomed_last(self) -> None:
        # It is the largest component, so a failed (OOM) round trip of it must
        # still leave the smaller one re-homed. The text encoder is not in the
        # list at all any more: it is not part of the pipeline.
        self.assertEqual(svc._MMAP_BACKED_COMPONENTS, ("vae", "transformer"))

    def test_vae_options_are_applied_and_reapplied_on_a_cache_hit(self) -> None:
        pipe = self._build(placement="full_gpu", vae_tiling=True, vae_slicing=False)
        self.assertTrue(pipe.vae.tiling)
        self.assertFalse(pipe.vae.slicing)
        same = self._build(placement="full_gpu", vae_tiling=False, vae_slicing=True)
        self.assertIs(same, pipe)
        self.assertFalse(pipe.vae.tiling)
        self.assertTrue(pipe.vae.slicing)

    def test_a_key_change_rebuilds_and_reports_the_previous_key(self) -> None:
        self._build("flux2_klein:a")
        with patch.object(self.service._model_manager, "mark_unloaded") as unloaded:
            self._build("flux2_klein:b")
        unloaded.assert_called_once_with("flux2_klein:a")
        self.assertEqual(self.service._active_key, "flux2_klein:b")

    def test_an_offload_placement_without_a_gpu_is_refused(self) -> None:
        with patch.object(hardware, "_resolve_selected_backend_device", lambda _f: "cpu"):
            with self.assertRaises(RuntimeError) as caught:
                self._build(placement="model_cpu_offload")
        self.assertIn("GPU", str(caught.exception))

    def test_a_missing_scheduler_is_a_named_error(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        with self.assertRaises(FileNotFoundError) as caught:
            self._build()
        self.assertIn("scheduler", str(caught.exception))

    def test_load_progress_is_reported_for_every_component(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        normalized = svc.normalize_flux2_klein_params(self.params())
        report = svc._progress_reporter(
            lambda *frame: frames.append(frame), "load", svc.LOAD_PHASE_STEPS
        )
        self.service._ensure_pipeline_locked(
            normalized, "flux2_klein:progress", report, region_hw=(128, 128)
        )
        self.assertTrue(all(phase == "load" for phase, _s, _t, _l in frames))
        # The pipeline is now the FIRST phase, so it owns steps 1-5; step 0 is
        # the caller's "preparing", 6 the warm-up and 7-9 the prompt phase.
        self.assertEqual(
            [step for _p, step, _t, _l in frames],
            [
                svc.LOAD_STEP_TRANSFORMER,
                svc.LOAD_STEP_TOKENIZER,
                svc.LOAD_STEP_VAE,
                svc.LOAD_STEP_SCHEDULER,
                svc.LOAD_STEP_PLACEMENT,
            ],
        )
        self.assertEqual([step for _p, step, _t, _l in frames], [1, 2, 3, 4, 5])
        self.assertTrue(all(total == svc.LOAD_PHASE_STEPS for _p, _s, total, _l in frames))

    def test_the_load_steps_are_reported_in_the_order_the_run_uses_them(self) -> None:
        # The step numbers ARE the order the user sees, so they must be strictly
        # increasing along the sequence a run performs: pipeline, warm-up, then
        # the text encoder.
        order = [
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
        ]
        self.assertEqual(order, sorted(order))
        self.assertEqual(len(set(order)), len(order))
        self.assertEqual(order[-1], svc.LOAD_PHASE_STEPS)
        # The encoder comes AFTER the transformer is placed — that is the whole
        # point of the new order.
        self.assertGreater(svc.LOAD_STEP_TEXT_ENCODER, svc.LOAD_STEP_PLACEMENT)


class PromptEncodingTests(_PlacementFixture):
    """The prompt phase: the encoder is loaded once and used once.

    It now runs AFTER the transformer has been placed on the accelerator, always
    in host memory, and by default stays there for the next prompt. Everything
    below pins that.
    """

    def test_encoding_loads_the_encoder_and_releases_it(self) -> None:
        embeds = self._encode(
            placement="encoder_cpu", prompt="a cat", unload_text_encoder_after_encode=True
        )
        self.assertIn("text_encoder", self.load_kwargs)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])
        # Released when asked: nothing of the encoder survives the phase.
        self.assertIsNone(self.service._text_encoder)
        self.assertIsNotNone(embeds["prompt"])
        self.assertIsNone(embeds["negative"])

    def test_the_encode_runs_under_no_grad(self) -> None:
        # Same reason as the decode: phase 1 does not go through
        # `pipeline.__call__`, and an 8B forward that builds an autograd graph
        # both wastes memory and poisons the cache with grad-carrying tensors.
        before = self.torch.no_grad.entered
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.assertGreater(self.torch.no_grad.entered, before)

    def test_the_encoder_stays_by_default_in_every_placement(self) -> None:
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                self.setUp()
                self._encode(placement=placement, prompt="a cat")
                self.assertIsNotNone(self.service._text_encoder)

    def test_the_encoder_always_encodes_in_host_memory(self) -> None:
        # Under the new load order the transformer is already on the card when
        # this phase runs, so the encoder can no longer join it there — 18.3 GB
        # plus 16.4 GB does not fit on the 34.2 GB reference card. It also never
        # asks the loader for a `device_map`, which is the load-straight-into-VRAM
        # path.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                self.setUp()
                self._encode(placement=placement, prompt="a cat")
                self.assertEqual(str(self.encode_calls[-1]["device"]), "cpu")
                self.assertNotIn("device_map", self.load_kwargs["text_encoder"])

    def _deep_encoder_config(self, layers: int = 36, *, layer_types: bool = True) -> None:
        """Give the fixture's encoder a config as a real klein checkout ships it."""
        config: dict[str, object] = {"num_hidden_layers": layers}
        if layer_types:
            config["layer_types"] = ["full_attention"] * layers
        (self.root / "text_encoder" / "config.json").write_text(
            json.dumps(config), encoding="utf-8"
        )

    def test_the_encoder_is_loaded_as_qwen3model_not_the_causal_lm(self) -> None:
        # The pipeline reads `output.hidden_states` only; the causal head would
        # compute a [1, 512, 151936] logits tensor per prompt on the host CPU.
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.assertIn("text_encoder", self.load_kwargs)
        self.assertNotIn("text_encoder_causal_lm", self.load_kwargs)

    def test_a_deep_encoder_is_loaded_truncated_with_matching_layer_types(self) -> None:
        # transformers 4.57 validates `num_hidden_layers` against
        # `len(layer_types)`, so the two kwargs must travel together.
        self._deep_encoder_config()
        self._encode(placement="encoder_cpu", prompt="a cat")
        kwargs = self.load_kwargs["text_encoder"]
        self.assertEqual(kwargs["num_hidden_layers"], svc.ENCODER_KEEP_LAYERS)
        self.assertEqual(len(kwargs["layer_types"]), svc.ENCODER_KEEP_LAYERS)

    def test_a_config_without_a_layer_count_is_loaded_whole(self) -> None:
        # The fixture tree's own `{}` config, and the branch a hand-made checkout
        # lands in: nothing is truncated and nothing raises.
        self._encode(placement="encoder_cpu", prompt="a cat")
        kwargs = self.load_kwargs["text_encoder"]
        self.assertNotIn("num_hidden_layers", kwargs)
        self.assertNotIn("layer_types", kwargs)

    def test_an_encoder_with_too_few_layers_is_refused(self) -> None:
        self._deep_encoder_config(svc.ENCODER_KEEP_LAYERS - 1)
        with self.assertRaises(ValueError) as caught:
            self._encode(placement="encoder_cpu", prompt="a cat")
        self.assertIn(str(svc.ENCODER_KEEP_LAYERS), str(caught.exception))
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_the_pipeline_is_told_which_hidden_states_to_stack(self) -> None:
        # Inherited defaults are what `ENCODER_KEEP_LAYERS` cannot survive: the
        # sibling `Flux2Pipeline` already uses (10, 20, 30).
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.assertEqual(
            self.encode_calls[-1]["text_encoder_out_layers"],
            svc.TEXT_ENCODER_OUT_LAYER_INDICES,
        )

    def test_the_encoder_is_bfloat16_whatever_dtype_the_request_asked_for(self) -> None:
        # It runs on the host CPU, where float16 has no native arithmetic; the
        # request's dtype governs the transformer and the VAE alone.
        for dtype in svc.VALID_DTYPES:
            with self.subTest(dtype=dtype):
                self.setUp()
                self._encode(placement="encoder_cpu", prompt="a cat", dtype=dtype)
                self.assertEqual(self.load_kwargs["text_encoder"]["dtype"], self.torch.bfloat16)

    def test_the_encoder_survives_a_change_of_the_request_dtype(self) -> None:
        # `_encoder_key` records the ENCODER's dtype, so switching the
        # transformer's precision must not evict a resident encoder.
        self._encode(placement="encoder_cpu", prompt="a cat", dtype="bfloat16")
        resident = self.service._text_encoder
        self.load_kwargs.clear()
        self._encode(placement="encoder_cpu", prompt="a dog", dtype="float16")
        self.assertIs(self.service._text_encoder, resident)
        self.assertEqual(self.load_kwargs, {})

    def test_a_cached_prompt_does_not_load_the_encoder(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.load_kwargs.clear()
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt="a cat", seed=99, mask_dilate_px=3)
        self.assertEqual(self.load_kwargs, {})
        self.assertEqual(self.encode_calls, [])

    def test_a_different_prompt_is_a_miss(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt="a dog")
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a dog"])

    def test_a_second_encode_does_not_re_read_the_tokenizer(self) -> None:
        # Every miss used to call `Qwen2TokenizerFast.from_pretrained` again,
        # which on a model that lives on a spinning disk is real I/O added to an
        # otherwise hot pipeline.
        self._encode(placement="encoder_cpu", prompt="a cat")
        self._encode(placement="encoder_cpu", prompt="a dog")
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat", "a dog"])
        self.assertEqual(self.load_calls["tokenizer"], 1)

    def test_guidance_above_one_also_encodes_the_empty_prompt(self) -> None:
        # Only on a checkpoint that does NOT declare itself distilled: there
        # classifier-free guidance really runs and the negative embedding is read.
        self.declare_distilled(False)
        embeds = self._encode(placement="encoder_cpu", prompt="a cat", guidance_scale=2.0)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat", ""])
        self.assertIsNotNone(embeds["negative"])

    def test_an_undeclared_checkpoint_still_honours_the_guidance_slider(self) -> None:
        # A hand-assembled folder of components carries no `model_index.json`.
        # The documented policy is "keep today's behaviour": the slider is live.
        self.declare_distilled(None)
        embeds = self._encode(placement="encoder_cpu", prompt="a cat", guidance_scale=2.0)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat", ""])
        self.assertIsNotNone(embeds["negative"])

    def test_a_distilled_checkpoint_never_encodes_a_negative_prompt(self) -> None:
        # The shipped klein 4B declares `is_distilled: true`, so
        # `do_classifier_free_guidance` is False whatever the slider says — and a
        # negative encode there is a full, wasted pass over the 16 GB encoder.
        with self.assertLogs(svc.log, level="INFO"):
            embeds = self._encode(placement="encoder_cpu", prompt="a cat", guidance_scale=7.0)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])
        self.assertIsNone(embeds["negative"])

    def test_the_cache_evicts_the_least_recently_used_entry(self) -> None:
        prompts = [f"prompt {index}" for index in range(svc.PROMPT_EMBED_CACHE_ENTRIES + 1)]
        for prompt in prompts:
            self._encode(placement="encoder_cpu", prompt=prompt)
        self.assertEqual(len(self.service._prompt_cache), svc.PROMPT_EMBED_CACHE_ENTRIES)
        self.encode_calls.clear()
        # The oldest one is gone and has to be encoded again; the newest is not.
        self._encode(placement="encoder_cpu", prompt=prompts[-1])
        self.assertEqual(self.encode_calls, [])
        self._encode(placement="encoder_cpu", prompt=prompts[0])
        self.assertEqual([call["prompt"] for call in self.encode_calls], [prompts[0]])

    def test_a_hit_refreshes_the_entry_so_it_is_not_evicted_next(self) -> None:
        prompts = [f"prompt {index}" for index in range(svc.PROMPT_EMBED_CACHE_ENTRIES)]
        for prompt in prompts:
            self._encode(placement="encoder_cpu", prompt=prompt)
        self._encode(placement="encoder_cpu", prompt=prompts[0])  # refresh the oldest
        self._encode(placement="encoder_cpu", prompt="one more")  # evicts prompts[1]
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt=prompts[0])
        self.assertEqual(self.encode_calls, [])

    def test_unload_keeps_the_prompt_cache_and_drops_the_encoder(self) -> None:
        self._encode(placement="full_gpu", prompt="a cat")
        cached = dict(self.service._prompt_cache)
        self.assertTrue(self.service.unload())
        self.assertIsNone(self.service._text_encoder)
        self.assertEqual(dict(self.service._prompt_cache), cached)

    def test_fp8_is_a_separate_cache_key(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.encode_calls.clear()
        with patch.object(pipeline, "_quantize_text_encoder_fp8", return_value=0):
            self._encode(placement="encoder_cpu", prompt="a cat", text_encoder_fp8=True)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])

    def test_fp8_quantizes_the_encoder_before_it_is_used(self) -> None:
        with patch.object(pipeline, "_quantize_text_encoder_fp8", return_value=1) as quantize:
            self._encode(placement="encoder_cpu", prompt="a cat", text_encoder_fp8=True)
        quantize.assert_called_once()

    def test_fp8_is_off_by_default_everywhere(self) -> None:
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(self.params(placement=placement))
                self.assertFalse(normalized["text_encoder_fp8"])

    def test_the_unload_default_is_off_in_every_placement(self) -> None:
        # The reorder moved the encoder behind the transformer's departure from
        # host memory, so keeping it costs RAM nothing else in the run wants and
        # buys an instant prompt change. The default is the same everywhere now,
        # placement included.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(self.params(placement=placement))
                self.assertFalse(normalized["unload_text_encoder_after_encode"])

    def test_an_explicit_unload_request_is_honoured(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", unload_text_encoder_after_encode=True)
        )
        self.assertTrue(normalized["unload_text_encoder_after_encode"])

    def test_an_explicit_flag_wins_over_the_placement_default(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="encoder_cpu", unload_text_encoder_after_encode=False)
        )
        self.assertFalse(normalized["unload_text_encoder_after_encode"])


class SingleFileTransformerPlacementTests(_PlacementFixture):
    """Regression cover for the «Минимум RAM» device mismatch (2026-09-02).

    `encoder_cpu` + `low_cpu_mem_usage` with a single-FILE transformer used to
    build a pipeline whose transformer sat in host memory while the VAE was on
    the accelerator: diffusers' `from_single_file` accepts `device_map` and
    discards it, and `_apply_placement` skipped its own move because a
    `device_map` had been passed. `DiffusionPipeline.device` then answered `cpu`,
    `_execution_device` followed, and the pipeline's VAE encode of the region
    died with "Input type (CPUBFloat16Type) and weight type (CUDABFloat16Type)".
    """

    single_file_transformer = True

    def setUp(self) -> None:
        super().setUp()
        #: Where the fake streaming loader leaves the transformer. Overridden by
        #: the test that reproduces a loader ignoring its placement.
        self.streamed_device = "cuda:0"
        #: One entry per `load_transformer_streaming` call, with its kwargs.
        self.streamed: list[dict[str, object]] = []
        module_cls = self.torch.nn.Module
        case = self

        def _stream(model_cls: object, source: object, **kwargs: object) -> object:
            case.streamed.append({"model_cls": model_cls, "source": str(source), **kwargs})
            progress = kwargs.get("progress")
            if callable(progress):
                progress(STREAM_TOTAL_BYTES // 2, STREAM_TOTAL_BYTES, "blocks.0.weight")
                progress(STREAM_TOTAL_BYTES, STREAM_TOTAL_BYTES, "blocks.1.weight")
            return module_cls("transformer", ptr=0x4000, device_type=case.streamed_device)

        # Patched on the module that DEFINES it, so `pipeline`'s
        # `streaming.load_transformer_streaming(...)` lookup sees the double.
        stream_patch = patch.object(streaming, "load_transformer_streaming", _stream)
        stream_patch.start()
        self.addCleanup(stream_patch.stop)

    def _shard_path(self) -> str:
        """A checkpoint the streaming loader must refuse: one part of a shard set."""
        shard = self.root / "flux2-klein-00001-of-00002.safetensors"
        _write_safetensors(shard, {"single_stream_modulation.lin.weight": {"dtype": "BF16"}})
        return str(shard)

    def test_a_streamable_checkpoint_goes_to_the_streaming_loader(self) -> None:
        # `low_cpu_mem_usage` + a whole-model `device_map` onto CUDA + one
        # `.safetensors` FILE is exactly the request the streaming loader serves,
        # and it keeps the host peak at one tensor instead of the whole 17 GiB.
        pipe = self._build(placement="encoder_cpu", low_cpu_mem_usage=True)
        self.assertEqual(len(self.streamed), 1)
        call = self.streamed[0]
        self.assertEqual(call["source"], self.paths["transformer_path"])
        self.assertEqual(call["device_map"], {"": "cuda:0"})
        self.assertIs(call["low_cpu_mem_usage"], True)
        # The ordinary loader was not even asked.
        self.assertNotIn("transformer_single", self.load_kwargs)
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        # The encoder is not part of this pipeline at all any more.
        self.assertIsNone(pipe.text_encoder)

    def test_a_streamed_transformer_is_still_placed_and_still_checked(self) -> None:
        # The invariant does not weaken for the new loader: `_apply_placement`
        # moves unconditionally (inside the staging patch) and
        # `_require_components_materialized` still has to pass afterwards. The
        # double deliberately leaves the transformer on the host to prove it.
        self.streamed_device = "cpu"
        pipe = self._build(placement="encoder_cpu", low_cpu_mem_usage=True)
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        self.assertEqual([depth for _dev, depth in pipe.transformer.moves], [1])
        svc._require_components_materialized(pipe, _FakeDevice("cuda:0"))

    def test_a_refused_checkpoint_falls_back_and_the_reason_is_logged(self) -> None:
        # `streaming.py` carries no fallback on purpose: the ordinary loader is
        # chosen HERE, and the reason has to be in the log or the choice is
        # invisible.
        with self.assertLogs(svc.log, level="INFO") as captured:
            pipe = self._build(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                transformer_path=self._shard_path(),
            )
        self.assertEqual(self.streamed, [])
        kwargs = self.load_kwargs["transformer_single"]
        self.assertEqual(kwargs["device"], "cuda:0")
        self.assertNotIn("device_map", kwargs)
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        reasons = [line for line in captured.output if "потоковая загрузка" in line.lower()]
        self.assertTrue(reasons, captured.output)
        self.assertIn("шардированного", reasons[0])

    def test_a_single_file_load_without_a_device_map_asks_for_no_device(self) -> None:
        self._build(placement="encoder_cpu")
        self.assertNotIn("device", self.load_kwargs["transformer_single"])

    def test_a_transformer_the_loader_left_on_the_host_is_still_placed(self) -> None:
        # The invariant that makes any future loader quirk harmless: placement
        # does not trust the loader kwargs, it moves what is not already there.
        module_cls = self.torch.nn.Module
        pipe = self.pipeline_cls(
            transformer=module_cls("transformer", ptr=0x4000),
            vae=_make_fake_vae(module_cls, object(), device_type="cuda:0"),
            text_encoder=module_cls("text_encoder", ptr=0x2000),
        )
        svc._apply_placement(pipe, "encoder_cpu", _FakeDevice("cuda:0"))
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        self.assertEqual([depth for _dev, depth in pipe.transformer.moves], [1])
        self.assertEqual(pipe.text_encoder.moves, [])

    def test_the_translation_only_covers_a_whole_model_device_map(self) -> None:
        self.assertEqual(svc._single_file_device({"": "cuda:0"}), "cuda:0")
        self.assertIsNone(svc._single_file_device(None))
        self.assertIsNone(svc._single_file_device({}))
        # A per-submodule map cannot be expressed as one `device` kwarg.
        self.assertIsNone(svc._single_file_device({"blocks.0": "cuda:0", "blocks.1": "cpu"}))


class ExecutionDeviceProbeTests(unittest.TestCase):
    """A run must not start on a device the pipeline was not placed on."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def _pipe(self, transformer_device: str) -> object:
        """A pipeline whose `_execution_device` follows diffusers' own rule.

        With no accelerate hooks the probe degrades to `DiffusionPipeline.device`,
        which is the device of the first component in SORTED signature order that
        is still an `nn.Module`. The encoder is `None` here, as it is in the real
        pipeline after the two-phase split, so the transformer decides — and a
        transformer left on the host makes the whole run execute there.
        """
        module_cls = self.torch.nn.Module

        class _Pipe(types.SimpleNamespace):
            @property
            def _execution_device(self) -> object:
                for name in ("text_encoder", "transformer", "vae"):
                    component = getattr(self, name, None)
                    if component is not None:
                        return component.device
                return _FakeDevice("cpu")

        return _Pipe(
            transformer=module_cls("transformer", ptr=0x4000, device_type=transformer_device),
            vae=_make_fake_vae(module_cls, object(), device_type="cuda:0"),
            text_encoder=None,
        )

    def test_a_transformer_left_on_the_host_is_refused(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            svc._require_execution_device(self._pipe("cpu"), _FakeDevice("cuda:0"))
        message = str(caught.exception)
        self.assertIn("transformer=cpu", message)
        self.assertIn("vae=cuda:0", message)

    def test_a_correctly_placed_pipeline_passes(self) -> None:
        svc._require_execution_device(self._pipe("cuda:0"), _FakeDevice("cuda:0"))

    def test_a_pipeline_without_the_property_is_not_probed(self) -> None:
        # A test double is not a `DiffusionPipeline`; the tripwire must not turn
        # its absence into a failure.
        svc._require_execution_device(types.SimpleNamespace(), _FakeDevice("cuda:0"))


# ---------------------------------------------------------------------------
# Single-file transformer loading
# ---------------------------------------------------------------------------
class SingleFileTransformerTests(_TempTreeCase):
    single_file_transformer = True

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[dict[str, object]] = []
        calls = self.calls

        class _Model:
            @staticmethod
            def from_single_file(path: str, **kwargs: object) -> str:
                calls.append({"path": path, **kwargs})
                return "transformer"

            @staticmethod
            def from_pretrained(path: str, **kwargs: object) -> str:
                calls.append({"path": path, "pretrained": True, **kwargs})
                return "transformer"

        self.model_cls = _Model

    def test_single_file_forces_guidance_embeds_off(self) -> None:
        svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        # klein has no `guidance_in` block, and diffusers would otherwise use the
        # flux-2-dev config, which does.
        self.assertIs(self.calls[0]["guidance_embeds"], False)

    def test_the_config_beside_the_checkpoint_is_used_and_the_hub_is_blocked(self) -> None:
        # `<checkpoint dir>/transformer/config.json` is the klein standalone
        # layout. Passing it as `config` is what keeps diffusers from resolving
        # the checkpoint to the gated `black-forest-labs/FLUX.2-dev` repo, and
        # `local_files_only` closes the network path a second time.
        svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        self.assertEqual(self.calls[0]["config"], str(self.root / "transformer"))
        self.assertIs(self.calls[0]["local_files_only"], True)

    def test_a_checkpoint_inside_the_transformer_folder_is_also_covered(self) -> None:
        # The other legitimate layout: the file sits IN the diffusers folder, so
        # the config is in the checkpoint's own directory rather than a subfolder.
        folder = self.root / "transformer"
        checkpoint = folder / "diffusion_pytorch_model.safetensors"
        _write_safetensors(checkpoint, {"single_stream_modulation.lin.weight": {"dtype": "BF16"}})

        svc._load_transformer(
            self.model_cls,
            str(checkpoint),
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        self.assertEqual(self.calls[0]["config"], str(folder))

    def test_a_missing_config_is_refused_with_instructions_and_no_hub_call(self) -> None:
        (self.root / "transformer" / "config.json").unlink()
        (self.root / "transformer").rmdir()

        with self.assertRaises(FileNotFoundError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )

        message = str(caught.exception)
        # What to do, where exactly, and why nothing is guessed instead.
        self.assertIn(str(self.root / "transformer" / "config.json"), message)
        self.assertIn("rope_theta", message)
        self.assertIn("FLUX.2-dev", message)
        # Every probed directory is named, so the user can see where to look.
        for candidate in svc.component_probe_order(
            svc.transformer_config_roots(Path(self.paths["transformer_path"])), "transformer"
        ):
            self.assertIn(str(candidate), message)
        # Refused before the loader ran: nothing could have reached the Hub.
        self.assertEqual(self.calls, [])

    def test_a_config_of_another_component_is_refused(self) -> None:
        # A VAE config found next to the checkpoint would build a different
        # architecture from the same weights instead of failing.
        (self.root / "transformer" / "config.json").write_text(
            json.dumps({"_class_name": "AutoencoderKLFlux2"}), encoding="utf-8"
        )
        with self.assertRaises(ValueError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("AutoencoderKLFlux2", str(caught.exception))
        self.assertEqual(self.calls, [])

    def test_an_unreadable_config_is_refused(self) -> None:
        (self.root / "transformer" / "config.json").write_text("{not json", encoding="utf-8")
        with self.assertRaises(ValueError):
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertEqual(self.calls, [])

    def test_an_fp8_scaled_checkpoint_is_refused_with_a_readable_error(self) -> None:
        _write_safetensors(
            Path(self.paths["transformer_path"]),
            {"blocks.0.weight": {"dtype": "F8_E4M3"}, "blocks.0.weight_scale": {"dtype": "F32"}},
        )
        with self.assertRaises(ValueError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("fp8_scaled", str(caught.exception))
        self.assertEqual(self.calls, [])

    def test_an_eligible_request_never_reaches_the_ordinary_loader(self) -> None:
        # The whole point of the branch: `from_single_file` reads the ENTIRE
        # checkpoint into host memory first, so an eligible request must not
        # touch it at all.
        streamed: list[dict[str, object]] = []

        def _stream(model_cls: object, source: object, **kwargs: object) -> str:
            streamed.append({"source": str(source), **kwargs})
            return "streamed"

        with patch.object(streaming, "load_transformer_streaming", _stream):
            result = svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map={"": "cuda:0"},
                low_cpu_mem_usage=True,
            )

        self.assertEqual(result, "streamed")
        self.assertEqual(self.calls, [])
        self.assertEqual(streamed[0]["source"], self.paths["transformer_path"])
        self.assertEqual(streamed[0]["device_map"], {"": "cuda:0"})

    def test_the_byte_callback_is_forwarded_without_the_tensor_name(self) -> None:
        # The wire's second level names the FILE, which the reporter already
        # bound; the loader's per-tensor key is dropped here rather than
        # flickering through the user's progress bar.
        seen: list[tuple[int, int]] = []

        def _stream(_model_cls: object, _source: object, **kwargs: object) -> str:
            progress = kwargs["progress"]
            assert callable(progress)
            progress(4, 8, "blocks.0.weight")
            return "streamed"

        with patch.object(streaming, "load_transformer_streaming", _stream):
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map={"": "cuda:0"},
                low_cpu_mem_usage=True,
                progress=lambda done, total: seen.append((done, total)),
            )
        self.assertEqual(seen, [(4, 8)])

    def test_no_callback_means_no_callback_is_handed_down(self) -> None:
        # `_ByteProgress` already tolerates `None`; handing it a lambda that
        # calls nothing would only hide a wiring mistake.
        handed: list[object] = []

        def _stream(_model_cls: object, _source: object, **kwargs: object) -> str:
            handed.append(kwargs["progress"])
            return "streamed"

        with patch.object(streaming, "load_transformer_streaming", _stream):
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map={"": "cuda:0"},
                low_cpu_mem_usage=True,
            )
        self.assertEqual(handed, [None])

    def test_an_ineligible_request_logs_the_reason_and_uses_the_ordinary_loader(self) -> None:
        # `low_cpu_mem_usage=False` is the plainest refusal there is, and the
        # sentence the predicate returns is the one that must reach the log.
        def _stream(*_args: object, **_kwargs: object) -> object:
            raise AssertionError("an ineligible request must not be streamed")

        with patch.object(streaming, "load_transformer_streaming", _stream):
            with self.assertLogs(svc.log, level="INFO") as captured:
                svc._load_transformer(
                    self.model_cls,
                    self.paths["transformer_path"],
                    dtype="bfloat16",
                    device_map=None,
                    low_cpu_mem_usage=False,
                )
        self.assertEqual(len(self.calls), 1)
        reasons = [line for line in captured.output if "потоковая загрузка" in line.lower()]
        self.assertTrue(reasons, captured.output)
        self.assertIn("low_cpu_mem_usage", reasons[0])

    def test_a_loader_failure_is_propagated_unchanged(self) -> None:
        # With a local config in hand there is no remedy left to suggest, so the
        # loader's own error must not be rewrapped into a different type.
        class _Failing:
            @staticmethod
            def from_single_file(_path: str, **_kwargs: object) -> object:
                raise OSError("checkpoint is truncated")

        with self.assertRaises(OSError) as caught:
            svc._load_transformer(
                _Failing,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("truncated", str(caught.exception))


class DirectoryTransformerTests(_TempTreeCase):
    """A diffusers folder must get the same treatment as a single file."""

    single_file_transformer = False

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[dict[str, object]] = []
        calls = self.calls

        class _Model:
            @staticmethod
            def from_pretrained(path: str, **kwargs: object) -> str:
                calls.append({"path": path, **kwargs})
                return "transformer"

            @staticmethod
            def from_single_file(_path: str, **_kwargs: object) -> object:
                raise AssertionError("a directory must not go through from_single_file")

        self.model_cls = _Model

    def _load(self) -> object:
        return svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )

    def test_a_directory_also_forces_guidance_embeds_off(self) -> None:
        # klein has no `guidance_in` block either way: a folder carrying the
        # flux-2-dev value of `guidance_embeds: true` must not build that
        # architecture just because it came from a directory.
        self._load()
        self.assertIs(self.calls[0]["guidance_embeds"], False)

    def test_a_plain_bf16_directory_is_loaded(self) -> None:
        _write_safetensors(
            Path(self.paths["transformer_path"]) / "diffusion_pytorch_model.safetensors",
            {"blocks.0.weight": {"dtype": "BF16"}},
        )
        self.assertEqual(self._load(), "transformer")

    def test_an_fp8_scaled_shard_is_refused_before_the_load(self) -> None:
        folder = Path(self.paths["transformer_path"])
        _write_safetensors(
            folder / "diffusion_pytorch_model-00001-of-00002.safetensors",
            {"blocks.0.weight": {"dtype": "BF16"}},
        )
        _write_safetensors(
            folder / "diffusion_pytorch_model-00002-of-00002.safetensors",
            {"blocks.1.weight": {"dtype": "F8_E4M3"}, "blocks.1.weight_scale": {"dtype": "F32"}},
        )
        (folder / "diffusion_pytorch_model.safetensors.index.json").write_text(
            json.dumps(
                {
                    "weight_map": {
                        "blocks.0.weight": "diffusion_pytorch_model-00001-of-00002.safetensors",
                        "blocks.1.weight": "diffusion_pytorch_model-00002-of-00002.safetensors",
                    }
                }
            ),
            encoding="utf-8",
        )
        (folder / "diffusion_pytorch_model.safetensors").unlink()

        with self.assertRaises(ValueError) as caught:
            self._load()
        self.assertIn("fp8_scaled", str(caught.exception))
        # Refused on the header alone: no multi-GiB load was started.
        self.assertEqual(self.calls, [])

    def test_every_shard_is_enumerated_once_index_or_not(self) -> None:
        folder = Path(self.paths["transformer_path"])
        (folder / "diffusion_pytorch_model.safetensors").unlink()
        for index in (1, 2):
            _write_safetensors(
                folder / f"model-0000{index}-of-00002.safetensors",
                {f"blocks.{index}.weight": {"dtype": "BF16"}},
            )
        _write_safetensors(folder / "extra.safetensors", {"lm_head.weight": {"dtype": "BF16"}})
        (folder / "model.safetensors.index.json").write_text(
            json.dumps({"weight_map": {"blocks.1.weight": "model-00001-of-00002.safetensors"}}),
            encoding="utf-8",
        )
        shards = svc.component_safetensors_shards(folder)
        self.assertEqual(len(shards), len(set(shards)))
        self.assertEqual(
            sorted(shard.name for shard in shards),
            [
                "extra.safetensors",
                "model-00001-of-00002.safetensors",
                "model-00002-of-00002.safetensors",
            ],
        )
        # The index is a hint about ordering, not an extra source of shards.
        self.assertEqual(shards[0].name, "model-00001-of-00002.safetensors")

    def test_a_malformed_index_does_not_break_the_scan(self) -> None:
        folder = Path(self.paths["transformer_path"])
        (folder / "model.safetensors.index.json").write_text("{not json", encoding="utf-8")
        self.assertEqual(
            [shard.name for shard in svc.component_safetensors_shards(folder)],
            ["diffusion_pytorch_model.safetensors"],
        )


# ---------------------------------------------------------------------------
# VAE decode: parking and OOM recovery
# ---------------------------------------------------------------------------
class DecodeRecoveryTests(_TempTreeCase):
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
            (
                hardware,
                "memory_snapshot",
                lambda *_args, **_kwargs: {"vram_total": 1 << 33, "vram_free": 1 << 30, "ram_total": 0, "ram_free": 0},
            ),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.decoded = object()
        self.latents = _FakeLatents()
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")

    def _pipe(self, *, oom_times: int = 0, **vae_kwargs: object) -> types.SimpleNamespace:
        return _fake_pipe(
            self.torch.nn.Module, self.decoded, oom_times=oom_times, **vae_kwargs
        )

    def _normalized(self, **overrides: object) -> dict[str, object]:
        return svc.normalize_flux2_klein_params(self.params(**overrides))

    def test_the_decode_runs_under_no_grad(self) -> None:
        # The decode is deliberately OUTSIDE `pipeline.__call__`, which is where
        # diffusers puts the `@torch.no_grad()`; without opening it ourselves the
        # VAE builds an autograd graph and `postprocess` dies on
        # "Can't call numpy() on Tensor that requires grad".
        before = self.torch.no_grad.entered
        svc._decode_once(self._pipe(), _FakeLatents())
        self.assertGreater(self.torch.no_grad.entered, before)

    def test_a_meta_resident_vae_decodes_on_the_execution_device(self) -> None:
        # `enable_sequential_cpu_offload` leaves the VAE's parameters on `meta`
        # between forwards, so `vae.device` is `meta` and moving the latents
        # there fails with "Cannot copy out of meta tensor; no data!".
        pipe = self._pipe()
        pipe.vae.device = _FakeDevice("meta")
        pipe._execution_device = _FakeDevice("cuda:0")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:0")

    def test_an_offloaded_vae_decodes_on_the_execution_device(self) -> None:
        # `enable_model_cpu_offload` leaves the VAE on the CPU with a hook, and
        # diffusers' `@apply_forward_hook` calls `pre_forward(self)` WITHOUT the
        # arguments — the weights move to the accelerator, our latents do not.
        pipe = self._pipe()
        pipe.vae._hf_hook = object()
        pipe._execution_device = _FakeDevice("cuda:0")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:0")

    def test_a_normally_placed_vae_decodes_on_its_own_device(self) -> None:
        # NOT the execution device: by decode time the transformer may be parked
        # on the host, which would make the probe answer `cpu`.
        pipe = self._pipe()
        pipe.vae.device = _FakeDevice("cuda:1")
        pipe._execution_device = _FakeDevice("cpu")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:1")

    def test_the_transformer_is_parked_before_the_decode_and_moved_back(self) -> None:
        pipe = self._pipe()
        normalized = self._normalized(
            placement="encoder_cpu", unload_transformer_before_vae=True
        )
        image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertFalse(recovered)
        self.assertTrue(applied["unload_transformer_before_vae"])
        # Off the device before the decode, back on it afterwards — and the way
        # back goes through the staging patch.
        self.assertEqual(
            [(str(target), depth) for target, depth in pipe.transformer.moves],
            [("cpu", 0), ("cuda:0", 1)],
        )
        self.assertEqual(pipe.vae.decode_calls, 1)

    def test_full_gpu_keeps_the_transformer_in_place_by_default(self) -> None:
        pipe = self._pipe()
        normalized = self._normalized(placement="full_gpu")
        _image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertEqual(pipe.transformer.moves, [])
        self.assertFalse(applied["unload_transformer_before_vae"])
        self.assertFalse(recovered)

    def test_an_oom_parks_the_transformer_and_retries_without_redenoising(self) -> None:
        pipe = self._pipe(oom_times=1)
        normalized = self._normalized(placement="full_gpu")
        image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertTrue(recovered)
        self.assertTrue(applied["unload_transformer_before_vae"])
        # Exactly two decode attempts, both from the same host copy of the
        # latents: the denoise is never repeated.
        self.assertEqual(pipe.vae.decode_calls, 2)
        self.assertEqual(str(pipe.transformer.moves[0][0]), "cpu")
        self.assertEqual(str(pipe.transformer.moves[-1][0]), "cuda:0")

    def test_a_second_oom_escalates_to_tiling_when_tiling_can_engage(self) -> None:
        # 192 latent cells a side is 1536 px, past the fixture VAE's 128-cell
        # threshold, so `enable_tiling()` here really does change what happens.
        pipe = self._pipe(oom_times=2)
        latents = _FakeLatents((1, 16, 192, 192))
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        image, applied, recovered = self.service._decode_locked(pipe, latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertTrue(recovered)
        self.assertTrue(applied["vae_tiling"])
        self.assertTrue(pipe.vae.tiling)
        # Slicing left the ladder: `decode` slices only when `z.shape[0] > 1` and
        # this service decodes a batch of one, so enabling it saves nothing and
        # claiming it would be written into the user's settings (KG-008).
        self.assertFalse(applied["vae_slicing"])
        self.assertFalse(pipe.vae.slicing)
        self.assertEqual(pipe.vae.decode_calls, 3)
        # The tiled attempt ran at the VAE's own thresholds; the last rung was
        # never needed.
        self.assertEqual(pipe.vae.thresholds_at_decode[-1], (128, 1024))

    def test_tiling_is_not_claimed_on_a_region_it_cannot_engage_for(self) -> None:
        # The common case: a 64x64 latent is a 512 px region, and diffusers enters
        # the tiled path only ABOVE 128 latent cells. Retrying with the flag on
        # would repeat the decode for nothing and persist a saving that never
        # happened — which is what the memory guard then plans against.
        pipe = self._pipe(oom_times=2)
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, self.latents, normalized)

        self.assertEqual(pipe.vae.decode_calls, 2)
        self.assertFalse(pipe.vae.tiling)
        message = str(caught.exception)
        # The refusal says WHY tiling is not an answer here, with the numbers.
        self.assertIn("1024", message)
        self.assertIn("512x512", message)

    def test_the_last_rung_lowers_both_thresholds_in_the_documented_ratio(self) -> None:
        # 96 latent cells (768 px) is below the VAE's 128-cell threshold, so the
        # only thing left to try is making the tile itself smaller.
        pipe = self._pipe(oom_times=2)
        latents = _FakeLatents((1, 16, 96, 96))
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        image, applied, recovered = self.service._decode_locked(pipe, latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertTrue(recovered)
        self.assertEqual(pipe.vae.decode_calls, 3)
        # BOTH thresholds moved, and in the ratio `tiled_decode` assumes:
        # it slices by the latent one and blends/crops by the sample one, so
        # lowering the latent side alone rebuilds the image as a mosaic of
        # shifted copies.
        self.assertEqual(
            pipe.vae.thresholds_at_decode[-1],
            (svc.EMERGENCY_TILE_LATENT_MIN_SIZE, svc.EMERGENCY_TILE_LATENT_MIN_SIZE * 8),
        )
        # And they are the VAE's own again afterwards: the change is per decode,
        # never a setting.
        self.assertEqual(
            (pipe.vae.tile_latent_min_size, pipe.vae.tile_sample_min_size), (128, 1024)
        )
        # `applied` carries the five persisted flags and no threshold.
        self.assertEqual(
            sorted(applied),
            [
                "text_encoder_fp8",
                "unload_text_encoder_after_encode",
                "unload_transformer_before_vae",
                "vae_slicing",
                "vae_tiling",
            ],
        )

    def test_the_last_rung_restores_the_thresholds_when_it_fails_too(self) -> None:
        pipe = self._pipe(oom_times=9)
        latents = _FakeLatents((1, 16, 96, 96))
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, latents, normalized)

        self.assertEqual(
            pipe.vae.thresholds_at_decode[-1],
            (svc.EMERGENCY_TILE_LATENT_MIN_SIZE, svc.EMERGENCY_TILE_LATENT_MIN_SIZE * 8),
        )
        self.assertEqual(
            (pipe.vae.tile_latent_min_size, pipe.vae.tile_sample_min_size), (128, 1024)
        )
        # The refusal names what was actually tried, not a generic "memory".
        self.assertIn("уменьшение тайла VAE", str(caught.exception))

    def test_a_vae_that_declares_no_thresholds_is_never_lowered(self) -> None:
        # Without `block_out_channels` the paired sample threshold cannot be
        # computed, and moving one of the two alone corrupts the image. Refusing
        # the rung is the only safe answer.
        pipe = self._pipe(oom_times=9, sample_size=None)
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        with self.assertRaises(RuntimeError):
            self.service._decode_locked(pipe, _FakeLatents((1, 16, 96, 96)), normalized)

        self.assertEqual(pipe.vae.decode_calls, 2)
        self.assertFalse(hasattr(pipe.vae, "tile_latent_min_size"))

    def test_an_unrecoverable_oom_reports_the_free_memory(self) -> None:
        pipe = self._pipe(oom_times=9)
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, self.latents, normalized)
        message = str(caught.exception)
        self.assertIn(str(1 << 30), message)
        # The transformer is still put back even on the failing path.
        self.assertEqual(str(pipe.transformer.moves[-1][0]), "cuda:0")

    def test_a_non_oom_failure_is_not_retried(self) -> None:
        pipe = self._pipe()

        def explode(*_args: object, **_kwargs: object) -> None:
            raise ValueError("bad latents")

        pipe.vae.decode = explode
        with self.assertRaises(ValueError):
            self.service._decode_locked(pipe, self.latents, self._normalized())

    def test_offload_placements_do_not_move_the_transformer_themselves(self) -> None:
        # Accelerate already returned it to host memory after the last forward.
        pipe = self._pipe()
        normalized = self._normalized(placement="model_cpu_offload")
        self.service._decode_locked(pipe, self.latents, normalized)
        self.assertEqual(pipe.transformer.moves, [])

    def test_rocm_style_runtime_errors_count_as_out_of_memory(self) -> None:
        self.assertTrue(_is_oom(svc, RuntimeError("HIP out of memory")))
        self.assertFalse(_is_oom(svc, RuntimeError("shape mismatch")))

    def _arm_failing_restore(self, key: str) -> object:
        """Register `key` as resident and make the transformer restore raise OOM."""
        manager = self.service._model_manager
        lease = manager.begin_model_use(key)
        lease.mark_loaded()
        self.service._active_key = key

        def explode(_pipe: object, _device: object) -> None:
            raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 18.00 GiB")

        restore_patch = patch.object(pipeline, "_restore_transformer_to_device", explode)
        restore_patch.start()
        self.addCleanup(restore_patch.stop)
        return manager

    def test_a_failed_restore_keeps_the_result_and_invalidates_the_cache(self) -> None:
        pipe = self._pipe()
        self.service._pipe = pipe
        manager = self._arm_failing_restore("flux2_klein:test")
        normalized = self._normalized(
            placement="encoder_cpu", unload_transformer_before_vae=True
        )

        image, _applied, _recovered = self.service._decode_locked(
            pipe, self.latents, normalized
        )

        # The decode succeeded, so its result must not be thrown away by a
        # failure that happens after it.
        self.assertIs(image, self.decoded)
        # ...but the cached pipeline no longer matches its key: its transformer
        # is on the host, so the next request must rebuild instead of hitting
        # the cache and failing on a device mismatch.
        self.assertIsNone(self.service._pipe)
        self.assertIsNone(self.service._active_key)
        self.assertTrue(manager.begin_model_use("flux2_klein:test").needs_load)

    def test_a_failed_restore_does_not_mask_a_failed_decode(self) -> None:
        pipe = self._pipe(oom_times=9)
        self.service._pipe = pipe
        manager = self._arm_failing_restore("flux2_klein:test")
        normalized = self._normalized(
            placement="encoder_cpu",
            unload_transformer_before_vae=True,
            vae_tiling=False,
            vae_slicing=False,
        )

        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, self.latents, normalized)
        # The decode's own diagnosis (with the free-memory figures) survives; the
        # restore failure does not replace it.
        self.assertIn(str(1 << 30), str(caught.exception))
        self.assertIsNone(self.service._pipe)
        self.assertTrue(manager.begin_model_use("flux2_klein:test").needs_load)


def _is_oom(module: object, exc: BaseException) -> bool:
    return module._is_out_of_memory(exc)




# ---------------------------------------------------------------------------
# Per-component residency and actions
# ---------------------------------------------------------------------------
class ComponentResidencyTests(unittest.TestCase):
    """`_module_residency` answers where the weights ARE, never a rounded guess."""

    def test_no_component_is_not_loaded(self) -> None:
        self.assertEqual(svc._module_residency(None), svc.RESIDENCY_NOT_LOADED)

    def test_every_tensor_on_the_host_is_ram(self) -> None:
        module = _ResidencyModule("cpu", "cpu", buffer_devices=("cpu",))
        self.assertEqual(svc._module_residency(module), svc.RESIDENCY_RAM)

    def test_every_tensor_on_the_card_is_gpu(self) -> None:
        module = _ResidencyModule("cuda", "cuda", buffer_devices=("cuda",))
        self.assertEqual(svc._module_residency(module), svc.RESIDENCY_GPU)

    def test_an_accelerate_hook_means_offloaded_whatever_the_parameters_say(self) -> None:
        # Under `model_cpu_offload` the parameters are on the host between
        # forwards and on the card during one; reporting that momentary truth as
        # `ram`/`gpu` would be a value the user cannot act on.
        self.assertEqual(
            svc._module_residency(_ResidencyModule("cpu", hooked=True)), svc.RESIDENCY_OFFLOADED
        )
        self.assertEqual(
            svc._module_residency(_ResidencyModule("cuda", hooked=True)), svc.RESIDENCY_OFFLOADED
        )

    def test_meta_parameters_are_offloaded_even_without_a_visible_hook(self) -> None:
        # `accelerate.cpu_offload` moves the parameters to `meta` and keeps the
        # bytes in a host weights map; the hook may sit on a submodule.
        module = _ResidencyModule("meta", "meta")
        self.assertEqual(svc._module_residency(module), svc.RESIDENCY_OFFLOADED)

    def test_a_split_component_is_mixed_and_is_not_rounded(self) -> None:
        for devices in (("cpu", "cuda"), ("meta", "cuda"), ("cpu", "meta"), ("cpu", "cuda", "meta")):
            with self.subTest(devices=devices):
                self.assertEqual(
                    svc._module_residency(_ResidencyModule(*devices)), svc.RESIDENCY_MIXED
                )

    def test_a_buffer_left_behind_makes_it_mixed_too(self) -> None:
        module = _ResidencyModule("cuda", "cuda", buffer_devices=("cpu",))
        self.assertEqual(svc._module_residency(module), svc.RESIDENCY_MIXED)

    def test_a_module_holding_no_weights_is_not_loaded(self) -> None:
        self.assertEqual(svc._module_residency(_ResidencyModule()), svc.RESIDENCY_NOT_LOADED)
        self.assertEqual(svc._module_residency(object()), svc.RESIDENCY_NOT_LOADED)


class ComponentActionsMatrixTests(unittest.TestCase):
    """`_component_actions` is the AUTHORITY; the invariants of the wire contract."""

    def _actions(self, component: str, residency: str, *, loaded: bool = True) -> list[str]:
        return svc._component_actions(component, residency, pipeline_loaded=loaded)

    def test_the_text_encoder_never_offers_to_gpu(self) -> None:
        # `_encode_prompts_locked` pins the host unconditionally, and the whole
        # load-order memory contract depends on it.
        for residency in (
            svc.RESIDENCY_NOT_LOADED,
            svc.RESIDENCY_RAM,
            svc.RESIDENCY_GPU,
            svc.RESIDENCY_OFFLOADED,
            svc.RESIDENCY_MIXED,
        ):
            for loaded in (False, True):
                with self.subTest(residency=residency, pipeline_loaded=loaded):
                    self.assertNotIn(
                        "to_gpu", self._actions("text_encoder", residency, loaded=loaded)
                    )

    def test_the_transformer_never_offers_a_warmup(self) -> None:
        # The warm-up forwards the VAE only; there is no transformer path to run.
        for residency in (
            svc.RESIDENCY_NOT_LOADED,
            svc.RESIDENCY_RAM,
            svc.RESIDENCY_GPU,
            svc.RESIDENCY_OFFLOADED,
            svc.RESIDENCY_MIXED,
        ):
            for loaded in (False, True):
                with self.subTest(residency=residency, pipeline_loaded=loaded):
                    self.assertNotIn(
                        "warmup", self._actions("transformer", residency, loaded=loaded)
                    )

    def test_load_and_unload_always_appear_on_both_pipeline_components(self) -> None:
        # They act on the pipeline as a whole: `_model_key` describes a whole
        # pipeline, so one of them alone cannot be dropped or loaded.
        for residency in (svc.RESIDENCY_GPU, svc.RESIDENCY_RAM, svc.RESIDENCY_OFFLOADED,
                          svc.RESIDENCY_MIXED):
            with self.subTest(residency=residency):
                transformer = self._actions("transformer", residency)
                vae = self._actions("vae", residency)
                self.assertIn("unload", transformer)
                self.assertIn("unload", vae)
                self.assertNotIn("load", transformer)
                self.assertNotIn("load", vae)
        for component in ("transformer", "vae"):
            with self.subTest(component=component):
                self.assertEqual(
                    self._actions(component, svc.RESIDENCY_NOT_LOADED, loaded=False), ["load"]
                )

    def test_the_text_encoder_loads_and_unloads_on_its_own(self) -> None:
        self.assertEqual(
            self._actions("text_encoder", svc.RESIDENCY_NOT_LOADED, loaded=True), ["load"]
        )
        self.assertEqual(self._actions("text_encoder", svc.RESIDENCY_RAM, loaded=False), ["unload"])

    def test_the_transformer_moves_only_between_the_two_plain_states(self) -> None:
        self.assertEqual(self._actions("transformer", svc.RESIDENCY_GPU), ["unload", "to_ram"])
        self.assertEqual(self._actions("transformer", svc.RESIDENCY_RAM), ["unload", "to_gpu"])
        # Under accelerate the move must be ABSENT, not merely disabled: the
        # parameters are on `meta` and the hooks would have to be removed first.
        self.assertEqual(self._actions("transformer", svc.RESIDENCY_OFFLOADED), ["unload"])
        self.assertEqual(self._actions("transformer", svc.RESIDENCY_MIXED), ["unload"])

    def test_the_vae_offers_a_warmup_only_where_it_can_run(self) -> None:
        self.assertEqual(self._actions("vae", svc.RESIDENCY_GPU), ["unload", "warmup"])
        for residency in (svc.RESIDENCY_RAM, svc.RESIDENCY_OFFLOADED, svc.RESIDENCY_MIXED):
            with self.subTest(residency=residency):
                self.assertEqual(self._actions("vae", residency), ["unload"])

    def test_the_vae_is_never_moved_by_hand(self) -> None:
        # No helper exists, and a host-resident VAE would silently make the next
        # decode run on the CPU (`_decode_once` follows `vae.device`).
        for residency in (svc.RESIDENCY_GPU, svc.RESIDENCY_RAM, svc.RESIDENCY_MIXED):
            with self.subTest(residency=residency):
                actions = self._actions("vae", residency)
                self.assertNotIn("to_ram", actions)
                self.assertNotIn("to_gpu", actions)

    def test_the_order_follows_the_wire_contract(self) -> None:
        for component in ("transformer", "vae"):
            for residency in (svc.RESIDENCY_GPU, svc.RESIDENCY_RAM):
                with self.subTest(component=component, residency=residency):
                    actions = self._actions(component, residency)
                    positions = [svc.COMPONENT_ACTIONS.index(name) for name in actions]
                    self.assertEqual(positions, sorted(positions))

if __name__ == "__main__":
    unittest.main()
