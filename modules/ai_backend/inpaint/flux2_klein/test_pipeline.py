"""
File: modules/ai_backend/inpaint/flux2_klein/test_pipeline.py

Purpose:
Unit tests for `pipeline.py`: the residency literal of a component's ledger
parts and the action matrix the client renders, the name validators, the
execution-device probe, the prompt encode's explicit hidden-state layers, the
warm-up decode, and ONE VAE decode attempt with its tiling options (the
request's flag, the lowered tile whose two thresholds move together and come
back, the gates that say whether tiling can engage at all).

Notes:
Stand-in VAEs record what the decode saw; torch runs on the CPU (the tests
skip without it). No GPU.
"""

from __future__ import annotations

import types
import unittest
from typing import Any

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import pipeline

try:
    import torch
except ImportError:  # pragma: no cover - the backend may run without torch
    torch = None  # type: ignore[assignment]


class ResidencyTests(unittest.TestCase):
    def test_the_literal_of_the_parts(self) -> None:
        self.assertEqual(pipeline.residency_of(0, 0), svc.RESIDENCY_NOT_LOADED)
        self.assertEqual(pipeline.residency_of(5, 0), svc.RESIDENCY_GPU)
        self.assertEqual(pipeline.residency_of(0, 5), svc.RESIDENCY_RAM)
        self.assertEqual(pipeline.residency_of(5, 5), svc.RESIDENCY_MIXED)

    def test_the_action_matrix(self) -> None:
        actions = pipeline._component_actions
        self.assertEqual(actions("text_encoder", svc.RESIDENCY_NOT_LOADED, pipeline_loaded=True), ["load"])
        self.assertEqual(actions("text_encoder", svc.RESIDENCY_GPU, pipeline_loaded=False), ["unload"], "never to_gpu")
        self.assertEqual(actions("transformer", svc.RESIDENCY_NOT_LOADED, pipeline_loaded=False), ["load"])
        self.assertEqual(actions("vae", svc.RESIDENCY_NOT_LOADED, pipeline_loaded=False), ["load"])
        self.assertEqual(actions("transformer", svc.RESIDENCY_GPU, pipeline_loaded=True), ["unload", "to_ram"])
        self.assertEqual(actions("transformer", svc.RESIDENCY_RAM, pipeline_loaded=True), ["unload", "to_gpu"])
        self.assertEqual(actions("transformer", svc.RESIDENCY_MIXED, pipeline_loaded=True), ["unload"])
        self.assertEqual(actions("vae", svc.RESIDENCY_GPU, pipeline_loaded=True), ["unload", "warmup"])
        self.assertEqual(actions("vae", svc.RESIDENCY_RAM, pipeline_loaded=True), ["unload"])

    def test_the_validators(self) -> None:
        self.assertEqual(pipeline._require_component_name(" vae "), "vae")
        self.assertEqual(pipeline._require_action_name("to_ram"), "to_ram")
        with self.assertRaises(ValueError):
            pipeline._require_component_name("tokenizer")
        with self.assertRaises(ValueError):
            pipeline._require_action_name("explode")


class ExecutionDeviceProbeTests(unittest.TestCase):
    def test_a_mismatch_is_refused_naming_the_components(self) -> None:
        pipe = types.SimpleNamespace(_execution_device="cpu", transformer=types.SimpleNamespace(device="cpu"), vae=types.SimpleNamespace(device="cuda:0"))
        with self.assertRaisesRegex(RuntimeError, "transformer=cpu"):
            pipeline._require_execution_device(pipe, "cuda:0")

    def test_a_match_or_no_probe_passes(self) -> None:
        pipeline._require_execution_device(types.SimpleNamespace(_execution_device="cuda:0"), "cuda:0")
        pipeline._require_execution_device(object(), "cuda:0")


@unittest.skipIf(torch is None, "needs torch")
class EncodePromptTests(unittest.TestCase):
    def test_the_hidden_state_layers_are_passed_explicitly_and_the_result_is_on_the_host(self) -> None:
        seen: list[dict[str, Any]] = []

        class Holder:
            def __init__(self, **components: Any) -> None:
                self.components = components

            def encode_prompt(self, **kwargs: Any) -> tuple[Any, Any]:
                seen.append({**kwargs, "grad": torch.is_grad_enabled()})
                return torch.ones(1, 4, 3), None

        out = pipeline._encode_prompt_phase(Holder, object(), object(), "a cat", 64, "cpu")
        self.assertEqual(seen[0]["text_encoder_out_layers"], svc.TEXT_ENCODER_OUT_LAYER_INDICES)
        self.assertEqual((seen[0]["max_sequence_length"], seen[0]["device"], seen[0]["grad"]), (64, "cpu", False))
        self.assertEqual(out.device.type, "cpu")


class _Vae:
    """A VAE stand-in with the klein tiling geometry; records every decode."""

    def __init__(self, *, sample_size: int | None = 1024, latent_channels: int | None = 16) -> None:
        self.decodes: list[tuple[bool, Any, Any]] = []
        self.use_tiling = False
        blocks = [128, 256, 512, 512]
        config: dict[str, Any] = {"block_out_channels": blocks}
        if latent_channels is not None:
            config["latent_channels"] = latent_channels
        self.config = types.SimpleNamespace(**config)
        if sample_size is not None:
            self.tile_sample_min_size = sample_size
            self.tile_latent_min_size = sample_size // 8
        self.dtype = torch.float32 if torch is not None else None
        self.device = torch.device("cpu") if torch is not None else None

    def enable_tiling(self) -> None:
        self.use_tiling = True

    def disable_tiling(self) -> None:
        self.use_tiling = False

    def decode(self, latents: Any, return_dict: bool = True) -> list[Any]:
        self.decodes.append((self.use_tiling, getattr(self, "tile_latent_min_size", None), getattr(self, "tile_sample_min_size", None)))
        return [latents]


class _Processor:
    def postprocess(self, image: Any, output_type: str = "pil") -> list[Any]:
        return ["decoded"]


@unittest.skipIf(torch is None, "needs torch")
class DecodeTests(unittest.TestCase):
    def test_the_requested_tiling_is_applied_and_reported_only_where_it_engages(self) -> None:
        vae = _Vae()
        image, tiled = pipeline.decode_region_latents(vae, _Processor(), torch.zeros(1, 16, 64, 64), tiling=True, lowered=False)
        self.assertEqual((image, tiled), ("decoded", False))
        self.assertEqual(vae.decodes, [(True, 128, 1024)])
        _image, tiled = pipeline.decode_region_latents(vae, _Processor(), torch.zeros(1, 16, 64, 144), tiling=True, lowered=False)
        self.assertTrue(tiled, "a latent side above 128 cells enters the tiled path")

    def test_the_lowered_tile_moves_both_thresholds_and_restores_them(self) -> None:
        vae = _Vae()
        _image, tiled = pipeline.decode_region_latents(vae, _Processor(), torch.zeros(1, 16, 96, 96), tiling=False, lowered=True)
        self.assertTrue(tiled)
        self.assertEqual(vae.decodes, [(True, 64, 512)])
        self.assertEqual((vae.tile_latent_min_size, vae.tile_sample_min_size), (128, 1024))

    def test_the_thresholds_come_back_when_the_decode_raises(self) -> None:
        vae = _Vae()

        def explode(latents: Any, return_dict: bool = True) -> Any:
            raise RuntimeError("HIP out of memory")

        vae.decode = explode  # type: ignore[method-assign]
        with self.assertRaises(RuntimeError):
            pipeline.decode_region_latents(vae, _Processor(), torch.zeros(1, 16, 96, 96), tiling=False, lowered=True)
        self.assertEqual((vae.tile_latent_min_size, vae.tile_sample_min_size), (128, 1024))

    def test_the_lowered_tile_never_runs_where_it_cannot_engage(self) -> None:
        small = _Vae()
        _image, tiled = pipeline.decode_region_latents(small, _Processor(), torch.zeros(1, 16, 32, 32), tiling=False, lowered=True)
        self.assertFalse(tiled, "a region inside one emergency tile is left alone")
        self.assertEqual(small.decodes, [(False, 128, 1024)])
        undeclared = _Vae(sample_size=None)
        self.assertIsNone(pipeline._emergency_tile_latent_size(undeclared, torch.zeros(1, 16, 96, 96)))

    def test_a_vae_without_a_sample_threshold_gets_none_back(self) -> None:
        vae = _Vae()
        del vae.tile_sample_min_size
        with pipeline._lowered_vae_tile_thresholds(vae, 64, 8):
            self.assertEqual(vae.tile_sample_min_size, 512)
        self.assertFalse(hasattr(vae, "tile_sample_min_size"))


@unittest.skipIf(torch is None, "needs torch")
class WarmupTests(unittest.TestCase):
    def test_one_tiny_decode_of_the_contract_size(self) -> None:
        vae = _Vae()
        seen: list[Any] = []
        vae.decode = lambda latents, return_dict=True: seen.append(tuple(latents.shape)) or [latents]  # type: ignore[method-assign]
        self.assertTrue(pipeline._warmup_vae_decode(vae, "cpu"))
        self.assertEqual(seen, [(1, 16, svc.WARMUP_LATENT_CELLS, svc.WARMUP_LATENT_CELLS)])

    def test_a_vae_without_latent_channels_only_synchronizes(self) -> None:
        vae = _Vae(latent_channels=None)
        with self.assertLogs(pipeline.log, level="WARNING"):
            self.assertFalse(pipeline._warmup_vae_decode(vae, "cpu"))
        self.assertEqual(vae.decodes, [])


if __name__ == "__main__":
    unittest.main()
