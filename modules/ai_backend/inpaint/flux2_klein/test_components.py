"""
File: modules/ai_backend/inpaint/flux2_klein/test_components.py

Purpose:
Unit tests for `components.py`: where the tokenizer and the scheduler are
discovered next to the paths the user supplies, how a component path is
normalized to the directory that actually holds a `config.json`, and what a
safetensors header is allowed to say.

Main responsibilities:
- verify the search roots and the probe order of `discover_component_dir`;
- verify `component_dir_for_path` and `component_safetensors_shards`;
- verify fp8-scaled checkpoints are recognized from the header alone;
- verify the encoder<->transformer width contract
  (`3 * hidden_size == joint_attention_dim`): both shipped variants pass, either
  variant's encoder against the other's transformer is refused with both files
  and both numbers named plus a structured log line, a single-file transformer
  and an encoder weights file resolve to the same configs the loaders read, and
  the guard stays SILENT for an empty path, a missing config or a config without
  the field — those are other faults with better diagnoses of their own.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc

from ._test_fixtures import _TempTreeCase, _write_safetensors


# ---------------------------------------------------------------------------
# Tokenizer / scheduler discovery
# ---------------------------------------------------------------------------
class ComponentDiscoveryTests(_TempTreeCase):
    def test_finds_tokenizer_and_scheduler_next_to_the_encoder(self) -> None:
        roots = svc.component_search_roots(self.paths)
        tokenizer = svc.discover_component_dir(roots, "tokenizer", svc._TOKENIZER_MARKERS)
        scheduler = svc.discover_component_dir(roots, "scheduler", ("scheduler_config.json",))
        self.assertEqual(tokenizer, self.root / "tokenizer")
        self.assertEqual(scheduler, self.root / "scheduler")

    def test_a_transformers_style_encoder_folder_is_its_own_tokenizer(self) -> None:
        (self.root / "tokenizer" / "tokenizer_config.json").unlink()
        (self.root / "tokenizer").rmdir()
        (self.root / "text_encoder" / "tokenizer.json").write_text("{}", encoding="utf-8")
        roots = svc.component_search_roots(self.paths)
        self.assertEqual(
            svc.discover_component_dir(roots, "tokenizer", svc._TOKENIZER_MARKERS),
            self.root / "text_encoder",
        )

    def test_missing_component_raises_with_the_searched_roots(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        roots = svc.component_search_roots(self.paths)
        with self.assertRaises(FileNotFoundError) as caught:
            svc._require_component_dir(roots, "scheduler", ("scheduler_config.json",), "планировщик")
        message = str(caught.exception)
        self.assertIn("scheduler_config.json", message)
        self.assertIn(str(self.root), message)


# ---------------------------------------------------------------------------
# Checkpoint inspection
# ---------------------------------------------------------------------------
class ComponentDirNormalizationTests(_TempTreeCase):
    """A weights file standing in for its folder, the way users actually pick it.

    `AutoencoderKLFlux2` and the Qwen3 encoder have no single-file loader, so
    selecting `diffusion_pytorch_model.safetensors` used to fail with the
    loader's own class list. The folder holding a file IS the component.
    """

    def test_file_beside_a_config_resolves_to_its_folder(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        weights = vae_dir / "diffusion_pytorch_model.safetensors"
        weights.write_bytes(b"")
        self.assertEqual(svc.component_dir_for_path(weights), vae_dir)

    def test_file_without_a_sibling_config_is_left_alone(self) -> None:
        lonely = self.root / "loose" / "diffusion_pytorch_model.safetensors"
        lonely.parent.mkdir(parents=True)
        lonely.write_bytes(b"")
        self.assertEqual(svc.component_dir_for_path(lonely), lonely)

    def test_a_folder_is_returned_unchanged(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        self.assertEqual(svc.component_dir_for_path(vae_dir), vae_dir)

    def test_vae_loader_accepts_a_file_inside_the_folder(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        weights = vae_dir / "diffusion_pytorch_model.safetensors"
        weights.write_bytes(b"")
        seen: list[str] = []

        class _Vae:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> str:
                seen.append(path)
                return "vae"

            @staticmethod
            def from_single_file(path: str, **_kwargs: object) -> str:
                raise AssertionError(f"single-file loader must not be reached for {path}")

        loaded = svc._load_vae(
            _Vae, str(weights), dtype=None, device_map=None, low_cpu_mem_usage=True
        )
        self.assertEqual(loaded, "vae")
        self.assertEqual(seen, [str(vae_dir)])

    def test_text_encoder_accepts_a_file_inside_the_folder(self) -> None:
        encoder_dir = Path(self.paths["text_encoder_path"])
        weights = encoder_dir / "model.safetensors"
        weights.write_bytes(b"")
        seen: list[str] = []

        class _Encoder:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> str:
                seen.append(path)
                return "encoder"

        loaded = svc._load_text_encoder(
            _Encoder, str(weights), dtype=None, device_map=None, low_cpu_mem_usage=True
        )
        self.assertEqual(loaded, "encoder")
        self.assertEqual(seen, [str(encoder_dir)])


class SafetensorsInspectionTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def test_reads_the_header(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(path, {"a.weight": {"dtype": "BF16", "shape": [2, 2]}})
        self.assertIn("a.weight", svc.read_safetensors_header(path))

    def test_rejects_a_non_safetensors_file(self) -> None:
        path = self.root / "not.safetensors"
        path.write_bytes(b"nope")
        with self.assertRaises(ValueError):
            svc.read_safetensors_header(path)

    def test_detects_fp8_scaled_by_scale_tensors(self) -> None:
        header = {"blocks.0.weight": {"dtype": "BF16"}, "blocks.0.weight_scale": {"dtype": "F32"}}
        self.assertTrue(svc.is_fp8_scaled_checkpoint(header))

    def test_detects_fp8_scaled_by_dtype(self) -> None:
        self.assertTrue(svc.is_fp8_scaled_checkpoint({"w": {"dtype": "F8_E4M3"}}))

    def test_a_plain_bf16_checkpoint_is_not_fp8(self) -> None:
        self.assertFalse(
            svc.is_fp8_scaled_checkpoint(
                {"__metadata__": {"format": "pt"}, "w": {"dtype": "BF16"}}
            )
        )


# ---------------------------------------------------------------------------
# Encoder <-> transformer compatibility
# ---------------------------------------------------------------------------
#: The two shipped variants, as `(text encoder hidden_size, transformer
#: joint_attention_dim)` read from the real `config.json` files on disk.
VARIANT_DIMENSIONS = {"9b": (4096, 12288), "4b": (2560, 7680)}

_COMPONENTS_LOGGER = "modules.ai_backend.inpaint.flux2_klein.components"


class EncoderTransformerCompatibilityTests(unittest.TestCase):
    """`3 * hidden_size == joint_attention_dim`, checked before any weight is read.

    A 4B transformer beside a 9B encoder passes every other check, passes the
    memory guard, and dies as a bare matmul shape error inside the denoise —
    after ~34 GB has been read from disk. These tests pin the check that stops
    it, and pin equally hard that it stays SILENT whenever it cannot prove a
    mismatch: absence is a different fault with a better diagnosis downstream.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def _encoder(self, hidden_size: object | None, *, name: str = "text_encoder") -> str:
        """A text-encoder directory carrying `hidden_size`; `None` omits the field."""
        folder = self.root / name
        folder.mkdir(parents=True, exist_ok=True)
        config: dict[str, object] = {"architectures": ["Qwen3ForCausalLM"]}
        if hidden_size is not None:
            config["hidden_size"] = hidden_size
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return str(folder)

    def _transformer(self, joint: object | None, *, name: str = "transformer") -> str:
        """A transformer directory carrying `joint_attention_dim`; `None` omits it."""
        folder = self.root / name
        folder.mkdir(parents=True, exist_ok=True)
        config: dict[str, object] = {"_class_name": "Flux2Transformer2DModel"}
        if joint is not None:
            config["joint_attention_dim"] = joint
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return str(folder)

    def test_the_layer_count_is_the_one_the_pipeline_concatenates(self) -> None:
        # diffusers stacks hidden states at layers (9, 18, 27); three of them.
        self.assertEqual(svc.TEXT_ENCODER_OUT_LAYERS, 3)

    def test_both_shipped_variants_are_self_consistent(self) -> None:
        for variant, (hidden, joint) in VARIANT_DIMENSIONS.items():
            with self.subTest(variant=variant):
                self.assertEqual(hidden * svc.TEXT_ENCODER_OUT_LAYERS, joint)
                svc.require_encoder_transformer_compatible(
                    {
                        "text_encoder_path": self._encoder(hidden, name=f"enc-{variant}"),
                        "transformer_path": self._transformer(joint, name=f"tr-{variant}"),
                    }
                )

    def test_a_9b_encoder_with_a_4b_transformer_is_refused(self) -> None:
        paths = {
            "text_encoder_path": self._encoder(4096),
            "transformer_path": self._transformer(7680),
        }
        with self.assertLogs(_COMPONENTS_LOGGER, level="ERROR") as logs:
            with self.assertRaises(ValueError) as caught:
                svc.require_encoder_transformer_compatible(paths)

        message = str(caught.exception)
        # Both sides are named, with their file AND their number, so the user can
        # see which of the two paths to change.
        self.assertIn("7680", message)
        self.assertIn("12288", message)
        self.assertIn("4096", message)
        self.assertIn(paths["text_encoder_path"], message)
        self.assertIn(paths["transformer_path"], message)
        # The remedy, and the reason there is no fallback.
        self.assertIn("2560", message)
        self.assertIn("FLUX.2-klein-9B", message)
        self.assertIn("FLUX.2-klein-4B", message)
        # A structured log line carries the same facts for the console half.
        self.assertTrue([line for line in logs.output if "joint_attention_dim=7680" in line])

    def test_a_4b_encoder_with_a_9b_transformer_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            svc.require_encoder_transformer_compatible(
                {
                    "text_encoder_path": self._encoder(2560),
                    "transformer_path": self._transformer(12288),
                }
            )

    def test_a_single_file_transformer_uses_the_config_beside_it(self) -> None:
        # The same search `_load_transformer` performs, so the guard and the
        # loader can never read two different configs.
        self._transformer(7680)
        checkpoint = self.root / "transformer" / "diffusion_pytorch_model.safetensors"
        checkpoint.write_bytes(b"")
        self.assertEqual(
            svc.transformer_config_dir(str(checkpoint)), self.root / "transformer"
        )
        with self.assertRaises(ValueError):
            svc.require_encoder_transformer_compatible(
                {"text_encoder_path": self._encoder(4096), "transformer_path": str(checkpoint)}
            )

    def test_an_encoder_weights_file_resolves_to_its_folder(self) -> None:
        encoder = Path(self._encoder(4096))
        weights = encoder / "model.safetensors"
        weights.write_bytes(b"")
        with self.assertRaises(ValueError):
            svc.require_encoder_transformer_compatible(
                {
                    "text_encoder_path": str(weights),
                    "transformer_path": self._transformer(7680),
                }
            )

    def test_it_stays_silent_when_a_path_is_empty(self) -> None:
        # A missing component is `require_text_encoder`'s / the loader's error,
        # with a much better message; this guard must not pre-empt it.
        transformer = self._transformer(7680)
        svc.require_encoder_transformer_compatible({"transformer_path": transformer})
        svc.require_encoder_transformer_compatible(
            {"text_encoder_path": "", "transformer_path": transformer}
        )
        svc.require_encoder_transformer_compatible(
            {"text_encoder_path": self._encoder(4096), "transformer_path": ""}
        )

    def test_it_stays_silent_when_a_config_is_missing_or_incomplete(self) -> None:
        good = self._transformer(7680)
        # No config at all next to the transformer.
        bare = self.root / "bare"
        bare.mkdir()
        svc.require_encoder_transformer_compatible(
            {"text_encoder_path": self._encoder(4096), "transformer_path": str(bare)}
        )
        self.assertIsNone(svc.transformer_config_dir(str(bare)))
        # A config without the field, on either side.
        svc.require_encoder_transformer_compatible(
            {
                "text_encoder_path": self._encoder(None, name="enc-nofield"),
                "transformer_path": good,
            }
        )
        svc.require_encoder_transformer_compatible(
            {
                "text_encoder_path": self._encoder(4096),
                "transformer_path": self._transformer(None, name="tr-nofield"),
            }
        )

    def test_a_non_numeric_or_boolean_dimension_is_not_a_dimension(self) -> None:
        # `bool` is a subclass of `int`: a hand-edited `true` must not read as 1.
        for value in (True, "7680", 0, -1, None):
            with self.subTest(value=value):
                svc.require_encoder_transformer_compatible(
                    {
                        "text_encoder_path": self._encoder(4096),
                        "transformer_path": self._transformer(value, name="tr-bad"),
                    }
                )

    def test_a_dimension_that_is_not_a_multiple_of_three_omits_the_hint(self) -> None:
        # `joint_attention_dim // 3` would be a lie there, so the message must
        # not offer it — but it must still refuse.
        with self.assertRaises(ValueError) as caught:
            svc.require_encoder_transformer_compatible(
                {
                    "text_encoder_path": self._encoder(4096),
                    "transformer_path": self._transformer(7681),
                }
            )
        self.assertNotIn("нужен энкодер с hidden_size", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
