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
  the field — those are other faults with better diagnoses of their own;
- verify the installed-package fallback of a GGUF component kept outside
  the packages: the tokenizer / scheduler roots (GGUF only, never for
  `model_index.json`; every installed package, 4B then 9B) and the transformer
  config, accepted only when consistent with the GGUF header, so a 9B file
  takes the 9B package and a 4B file the 4B one (the recorded 9B header,
  `fixtures/flux2_9b_gguf_headers.json`, written header-only);
- verify the recorded 9B transformer header: strict match with the 9B config,
  refusal of the 4B one, BFL names that convert onto the 9B model;
- verify the text encoder's config kwargs: the truncation, and the RoPE
  normalization of a transformers-v5 config (the uncensored 9B encoder's
  `rope_parameters`: a real `Qwen3Config` gets `rope_theta == 1e6`; a stock
  config gets no kwarg; any other RoPE type is refused).
"""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, conversion, large_model

from ._test_fixtures import _make_model_tree, _TempTreeCase, _write_safetensors, load_9b_config, load_9b_header_fixture, write_header_only_gguf


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


@unittest.skipUnless(importlib.util.find_spec("gguf") is not None, "needs the gguf package")
class GgufPackageFallbackTests(_TempTreeCase):
    """A GGUF component outside the package borrows from the installed 4B package, and only what fits."""

    #: The transformer `config.json` the small BFL GGUF below describes.
    CONFIG = {"_class_name": "Flux2Transformer2DModel", "num_layers": 1, "num_single_layers": 2, "num_attention_heads": 2, "attention_head_dim": 16, "in_channels": 8, "joint_attention_dim": 24}

    def setUp(self) -> None:
        super().setUp()
        from modules.ai_backend.runtime._gguf_test_fixtures import Q, make_tensor, write_gguf

        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        self.gguf = Path(outside.name) / "sd_models" / "klein-q8.gguf"
        self.gguf.parent.mkdir(parents=True)
        write_gguf(
            self.gguf,
            [
                make_tensor("img_in.weight", Q.BF16, (32, 8), seed=1),
                make_tensor("txt_in.weight", Q.BF16, (32, 24), seed=2),
                make_tensor("double_blocks.0.img_attn.qkv.weight", Q.Q8_0, (96, 32), seed=3),
                make_tensor("single_blocks.0.linear1.weight", Q.Q8_0, (64, 32), seed=4),
                make_tensor("single_blocks.1.linear1.weight", Q.Q8_0, (64, 32), seed=5),
            ],
        )

    def _install(self, subdir: str, files: dict[str, object]) -> Path:
        folder = self.installed_package / subdir
        folder.mkdir(parents=True)
        for name, content in files.items():
            (folder / name).write_text(json.dumps(content), encoding="utf-8")
        return folder

    def test_the_package_supplies_the_tokenizer_only_for_a_gguf_component(self) -> None:
        for name in ("tokenizer/tokenizer_config.json", "scheduler/scheduler_config.json"):
            (self.root / name).unlink()
        tokenizer = self._install("tokenizer", {"tokenizer_config.json": {}})
        paths = {**self.paths, "transformer_path": str(self.gguf)}
        roots = svc.component_search_roots(paths)
        self.assertEqual(roots[-1], self.installed_package, "appended last")
        self.assertEqual(svc.discover_component_dir(roots, "tokenizer", svc._TOKENIZER_MARKERS), tokenizer)
        self.assertNotIn(self.installed_package, svc.component_search_roots(self.paths), "no GGUF, no fallback")
        self.assertNotIn(self.installed_package, svc.model_index_search_roots(paths), "the manifest is never borrowed")

    def test_a_consistent_package_config_is_borrowed(self) -> None:
        folder = self._install("transformer", {"config.json": self.CONFIG})
        self.assertEqual(components.find_transformer_config_dir(self.gguf), folder)
        self.assertEqual(large_model.transformer_config_dir(str(self.gguf)), folder)

    def test_an_inconsistent_package_config_is_refused_with_the_reason(self) -> None:
        self._install("transformer", {"config.json": {**self.CONFIG, "num_single_layers": 20}})
        self.assertIsNone(components.find_transformer_config_dir(self.gguf))
        with self.assertRaises(FileNotFoundError) as caught:
            large_model.transformer_config_dir(str(self.gguf))
        message = str(caught.exception)
        self.assertIn("одиночных блоков: в файле 2, в config.json 20", message)
        self.assertIn(str(self.installed_package / "transformer"), message)

    def test_an_inconsistent_config_beside_the_file_is_skipped_for_the_package(self) -> None:
        beside = self.gguf.parent / "transformer"
        beside.mkdir()
        (beside / "config.json").write_text(json.dumps({**self.CONFIG, "num_layers": 8}), encoding="utf-8")
        folder = self._install("transformer", {"config.json": self.CONFIG})
        self.assertEqual(components.find_transformer_config_dir(self.gguf), folder)

    def test_the_package_fallback_needs_every_required_field(self) -> None:
        self._install("transformer", {"config.json": {key: value for key, value in self.CONFIG.items() if key != "num_layers"}})
        self.assertIsNone(components.find_transformer_config_dir(self.gguf))


#: The shipped FLUX.2 klein 4B `transformer/config.json`, reduced to the fields
#: the GGUF config match reads (recorded from the installed 4B package).
KLEIN_4B_TRANSFORMER_CONFIG = {"_class_name": "Flux2Transformer2DModel", "attention_head_dim": 128, "in_channels": 128, "joint_attention_dim": 7680, "num_attention_heads": 24, "num_layers": 5, "num_single_layers": 20}


def _write_4b_shaped_transformer_gguf(path: Path) -> Path:
    """A header-only GGUF with the 4B transformer's block counts and widths (5 double, 20 single blocks, width 3072)."""
    tensors: list[list[object]] = [["img_in.weight", [128, 3072], 30], ["txt_in.weight", [7680, 3072], 30]]
    tensors += [[f"double_blocks.{index}.img_attn.qkv.weight", [3072, 9216], 8] for index in range(5)]
    tensors += [[f"single_blocks.{index}.linear1.weight", [3072, 27648], 8] for index in range(20)]
    return write_header_only_gguf(path, {"general.architecture": "flux"}, tensors)


@unittest.skipUnless(importlib.util.find_spec("gguf") is not None, "needs the gguf package")
class NineBPackageFallbackTests(_TempTreeCase):
    """Both klein packages installed: a GGUF transformer outside them takes the config of the package it strictly matches (D18)."""

    def setUp(self) -> None:
        super().setUp()
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        folder = Path(outside.name) / "sd_models"
        header = load_9b_header_fixture("f9_Q4_K_M.head")
        self.gguf_9b = write_header_only_gguf(folder / "flux-2-klein-9b-Q4_K_M.gguf", header["metadata"], header["tensors"])  # type: ignore[arg-type]
        self.gguf_4b = _write_4b_shaped_transformer_gguf(folder / "flux-2-klein-4b-Q8_0.gguf")

    def _install(self, package: Path, config: dict[str, object]) -> Path:
        folder = package / "transformer"
        folder.mkdir(parents=True)
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return folder

    def test_each_file_takes_its_own_package(self) -> None:
        folder_4b = self._install(self.installed_package, KLEIN_4B_TRANSFORMER_CONFIG)
        folder_9b = self._install(self.installed_package_9b, load_9b_config("transformer"))
        with self.assertLogs(components.log, "DEBUG") as logs:
            self.assertEqual(components.find_transformer_config_dir(self.gguf_9b), folder_9b)
        self.assertTrue(any("из установленного пакета FLUX.2-klein-9B" in line for line in logs.output), logs.output)
        self.assertTrue(any("двойных блоков: в файле 8, в config.json 5" in line for line in logs.output), "the 4B package is probed first and rejected")
        self.assertEqual(components.find_transformer_config_dir(self.gguf_4b), folder_4b)
        self.assertEqual(large_model.transformer_config_dir(str(self.gguf_9b)), folder_9b)

    def test_only_the_other_package_installed_is_a_refusal_naming_it(self) -> None:
        self._install(self.installed_package, KLEIN_4B_TRANSFORMER_CONFIG)
        self.assertIsNone(components.find_transformer_config_dir(self.gguf_9b))
        with self.assertRaises(FileNotFoundError) as caught:
            large_model.transformer_config_dir(str(self.gguf_9b))
        message = str(caught.exception)
        self.assertIn(str(self.installed_package / "transformer"), message)
        self.assertIn("двойных блоков: в файле 8, в config.json 5", message)
        self.assertIn("(4B или 9B)", message)

    def test_every_installed_package_joins_the_tokenizer_search_in_order(self) -> None:
        self.installed_package.mkdir(parents=True)
        self.installed_package_9b.mkdir(parents=True)
        roots = svc.component_search_roots({**self.paths, "transformer_path": str(self.gguf_9b)})
        self.assertEqual(roots[-2:], [self.installed_package, self.installed_package_9b])
        self.assertEqual(components.installed_klein_package_roots(), [self.installed_package, self.installed_package_9b])
        self.assertNotIn(self.installed_package_9b, svc.component_search_roots(self.paths), "no GGUF, no fallback")

    def test_the_package_names_are_the_catalog_directories(self) -> None:
        # Mirrored by the Rust drift guard over `Flux2Variant::dir_name()`.
        self.assertEqual(svc.KLEIN_PACKAGE_DIR_NAMES, ("FLUX.2-klein-4B", "FLUX.2-klein-9B"))


@unittest.skipUnless(importlib.util.find_spec("gguf") is not None, "needs the gguf package")
class NineBTransformerHeaderTests(unittest.TestCase):
    """The recorded unsloth 9B transformer header against the 9B and 4B configs, and its names against the converter."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        header = load_9b_header_fixture("f9_Q4_K_M.head")
        self.path = write_header_only_gguf(Path(tmp.name) / "flux-2-klein-9b-Q4_K_M.gguf", header["metadata"], header["tensors"])  # type: ignore[arg-type]
        self.layout = components.gguf_layout(self.path)

    def test_the_9b_config_matches_strictly_and_the_4b_one_does_not(self) -> None:
        self.assertEqual(len(self.layout.spans), 201)
        self.assertIsNone(components._gguf_transformer_mismatch(load_9b_config("transformer"), self.layout, True))
        reason = components._gguf_transformer_mismatch(KLEIN_4B_TRANSFORMER_CONFIG, self.layout, True)
        self.assertEqual(reason, "двойных блоков: в файле 8, в config.json 5")

    def test_the_names_are_the_bfl_layout_without_a_prefix(self) -> None:
        names = [span.name for span in self.layout.spans]
        conversion.require_known_layout(names, source=self.path)
        self.assertTrue(all(conversion.is_bfl_key(name) for name in names), [name for name in names if not conversion.is_bfl_key(name)])

    @unittest.skipUnless(all(importlib.util.find_spec(name) is not None for name in ("torch", "diffusers", "accelerate")), "needs torch, diffusers and accelerate")
    def test_the_names_convert_onto_the_9b_model_and_every_type_dequantizes(self) -> None:
        import torch
        from accelerate import init_empty_weights
        from diffusers import Flux2Transformer2DModel

        config = {key: value for key, value in load_9b_config("transformer").items() if not key.startswith("_")}
        with init_empty_weights():
            model = Flux2Transformer2DModel.from_config(config)
        produced: set[str] = set()
        for span in self.layout.spans:
            placeholder = torch.empty(span.shape, dtype=getattr(torch, span.torch_dtype_name), device="meta")
            produced.update(conversion.convert_bfl_tensor(span.name, placeholder))
        self.assertEqual(produced, set(model.state_dict()))
        large_model._require_dequantizable_gguf(self.layout.spans, source=self.path)


class CheckpointIsDistilledTests(_TempTreeCase):
    """`model_index.json` as a TRI-STATE: declared true, declared false, unknown."""

    def test_the_shipped_manifest_declares_the_checkpoint_distilled(self) -> None:
        self.assertIs(svc.checkpoint_is_distilled(self.paths), True)

    def test_a_manifest_declaring_false_is_reported_as_false(self) -> None:
        self.declare_distilled(False)
        self.assertIs(svc.checkpoint_is_distilled(self.paths), False)

    def test_no_manifest_at_all_is_unknown(self) -> None:
        self.declare_distilled(None)
        self.assertIsNone(svc.checkpoint_is_distilled(self.paths))

    def test_a_manifest_without_the_field_is_unknown(self) -> None:
        (self.root / "model_index.json").write_text(
            json.dumps({"_class_name": "Flux2KleinPipeline"}), encoding="utf-8"
        )
        self.assertIsNone(svc.checkpoint_is_distilled(self.paths))

    def test_a_non_boolean_declaration_is_unknown_and_logged(self) -> None:
        # `bool` is a subclass of `int`, and a hand-edited `"true"` is a string:
        # neither may be coerced into an answer about compute.
        for value in ("true", 1, [True]):
            with self.subTest(value=value):
                self.declare_distilled(value)
                with self.assertLogs(svc.log, level="INFO"):
                    self.assertIsNone(svc.checkpoint_is_distilled(self.paths))

    def test_a_malformed_manifest_is_unknown_and_never_raises(self) -> None:
        # The flag can only make a run cheaper, so a broken metadata file must
        # not be able to kill a generation.
        (self.root / "model_index.json").write_text("{ not json", encoding="utf-8")
        with self.assertLogs(svc.log, level="INFO"):
            self.assertIsNone(svc.checkpoint_is_distilled(self.paths))

    def test_only_the_transformer_and_vae_roots_are_searched(self) -> None:
        # The answer becomes part of what the resident pipeline IS, so it must be
        # a function of the paths the pipeline handle's key carries. The text
        # encoder is another handle, so a manifest reachable through it alone
        # must not decide.
        self.declare_distilled(None)
        encoder_only = self.root / "elsewhere"
        (encoder_only / "text_encoder").mkdir(parents=True)
        (encoder_only / "model_index.json").write_text(
            json.dumps({"is_distilled": True}), encoding="utf-8"
        )
        paths = dict(self.paths)
        paths["text_encoder_path"] = str(encoder_only / "text_encoder")
        self.assertIsNone(svc.checkpoint_is_distilled(paths))
        self.assertNotIn(encoder_only, svc.model_index_search_roots(paths))

    def test_a_single_file_transformer_finds_the_manifest_beside_it(self) -> None:
        # The layout a standalone klein release ships: the checkpoint in the
        # repository root, `model_index.json` next to it.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            paths = _make_model_tree(root, single_file_transformer=True)
            self.assertIs(svc.checkpoint_is_distilled(paths), True)


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
        self.assertEqual(svc.component_dir_for_path(weights), vae_dir)
        # The manager's VAE files are the folder's shards, whichever path was picked.
        self.assertEqual(large_model.vae_files(str(weights)), large_model.vae_files(str(vae_dir)))

    def test_text_encoder_accepts_a_file_inside_the_folder(self) -> None:
        encoder_dir = Path(self.paths["text_encoder_path"])
        weights = encoder_dir / "model.safetensors"
        self.assertEqual(svc.component_dir_for_path(weights), encoder_dir)
        self.assertEqual(large_model.encoder_files(str(weights)), [weights])


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
        config: dict[str, object] = {"architectures": ["Qwen3Model"]}
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
        # The same search `large_model.transformer_config_dir` performs, so the guard and the
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


# ---------------------------------------------------------------------------
# Text-encoder truncation
# ---------------------------------------------------------------------------
class EncoderTruncationKwargsTests(unittest.TestCase):
    """`text_encoder_truncation_kwargs` decides per config, and never clamps."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def _encoder(self, config: dict[str, object]) -> Path:
        folder = self.root / "text_encoder"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return folder

    def test_a_deeper_encoder_is_cut_and_its_layer_types_are_cut_with_it(self) -> None:
        # Passing `num_hidden_layers` alone raises in transformers 4.57:
        # `layer_type_validation` compares it against `len(config.layer_types)`.
        keep = svc.ENCODER_KEEP_LAYERS
        folder = self._encoder(
            {"num_hidden_layers": 36, "layer_types": ["full_attention"] * 36}
        )
        kwargs = svc.text_encoder_truncation_kwargs(folder)
        self.assertEqual(kwargs["num_hidden_layers"], keep)
        self.assertEqual(len(kwargs["layer_types"]), keep)
        self.assertEqual(kwargs["layer_types"], ["full_attention"] * keep)

    def test_a_config_without_layer_types_gets_only_the_count(self) -> None:
        folder = self._encoder({"num_hidden_layers": 36})
        kwargs = svc.text_encoder_truncation_kwargs(folder)
        self.assertEqual(kwargs, {"num_hidden_layers": svc.ENCODER_KEEP_LAYERS})

    def test_an_encoder_of_exactly_the_needed_depth_is_not_touched(self) -> None:
        folder = self._encoder({"num_hidden_layers": svc.ENCODER_KEEP_LAYERS})
        self.assertEqual(svc.text_encoder_truncation_kwargs(folder), {})

    def test_too_few_layers_is_refused_and_never_clamped(self) -> None:
        # Clamping would feed the pipeline `model.norm`-ed states where raw layer
        # outputs belong — a silently wrong embedding.
        folder = self._encoder({"num_hidden_layers": svc.ENCODER_KEEP_LAYERS - 1})
        with self.assertRaises(ValueError) as caught:
            svc.text_encoder_truncation_kwargs(folder)
        message = str(caught.exception)
        self.assertIn(str(svc.ENCODER_KEEP_LAYERS - 1), message)
        self.assertIn(str(svc.ENCODER_KEEP_LAYERS), message)
        self.assertIn("config.json", message)

    def test_a_config_without_the_field_loads_the_encoder_whole(self) -> None:
        # The `{}` config every fixture tree writes, and a real one whose field
        # we cannot read: nothing is truncated and the reason is logged.
        folder = self._encoder({})
        with self.assertLogs(svc.log, level="INFO"):
            self.assertEqual(svc.text_encoder_truncation_kwargs(folder), {})

    def test_the_kept_depth_is_exactly_one_above_the_last_layer_read(self) -> None:
        # `hidden_states[n]` for `n == num_hidden_layers` is the `model.norm`-ed
        # output, so the `+ 1` is the exact minimum and not a safety margin.
        self.assertEqual(
            svc.ENCODER_KEEP_LAYERS, max(svc.TEXT_ENCODER_OUT_LAYER_INDICES) + 1
        )
        self.assertEqual(svc.TEXT_ENCODER_OUT_LAYERS, len(svc.TEXT_ENCODER_OUT_LAYER_INDICES))


class EncoderRopeKwargsTests(unittest.TestCase):
    """`text_encoder_rope_kwargs` / `text_encoder_config_kwargs`: a transformers-v5 RoPE base is passed explicitly (D17)."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def _encoder(self, config: dict[str, object], name: str = "text_encoder") -> Path:
        folder = self.root / name
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "config.json").write_text(json.dumps(config), encoding="utf-8")
        return folder

    def test_the_uncensored_config_gets_its_base_and_the_truncation(self) -> None:
        folder = self._encoder(load_9b_config("uncensored_text_encoder"))
        self.assertEqual(svc.text_encoder_rope_kwargs(folder), {"rope_theta": 1_000_000.0})
        with self.assertLogs(components.log, "INFO") as logs:
            kwargs = svc.text_encoder_config_kwargs(folder, svc.ENCODER_KEEP_LAYERS)
        self.assertEqual(kwargs["rope_theta"], 1_000_000.0)
        self.assertEqual(kwargs["num_hidden_layers"], svc.ENCODER_KEEP_LAYERS)
        self.assertEqual(len(kwargs["layer_types"]), svc.ENCODER_KEEP_LAYERS)
        self.assertTrue(any("rope_theta = 1000000.0" in line for line in logs.output), logs.output)

    def test_a_stock_config_gets_no_rope_kwarg(self) -> None:
        for config in (load_9b_config("text_encoder"), {"num_hidden_layers": 36}, {}):
            with self.subTest(config=sorted(config)):
                folder = self._encoder(config)
                self.assertEqual(svc.text_encoder_rope_kwargs(folder), {})
                self.assertNotIn("rope_theta", svc.text_encoder_config_kwargs(folder))

    def test_an_agreeing_top_level_base_needs_nothing(self) -> None:
        folder = self._encoder({"rope_theta": 1000000, "rope_parameters": {"rope_type": "default", "rope_theta": 1000000.0}})
        self.assertEqual(svc.text_encoder_rope_kwargs(folder), {})

    def test_every_other_rope_is_refused_by_name(self) -> None:
        cases = {
            "yarn": {"rope_parameters": {"rope_type": "yarn", "rope_theta": 1000000, "factor": 4.0}},
            "no type": {"rope_parameters": {"rope_theta": 1000000}},
            "no theta": {"rope_parameters": {"rope_type": "default"}},
            "bool theta": {"rope_parameters": {"rope_type": "default", "rope_theta": True}},
            "not an object": {"rope_parameters": [1000000]},
            "contradiction": {"rope_theta": 10000, "rope_parameters": {"rope_type": "default", "rope_theta": 1000000}},
        }
        for what, config in cases.items():
            with self.subTest(what=what):
                folder = self._encoder(config, name=what.replace(" ", "_"))
                with self.assertRaises(ValueError) as caught:
                    svc.text_encoder_config_kwargs(folder)
                self.assertIn(str(folder / "config.json"), str(caught.exception))
        with self.assertRaisesRegex(ValueError, "'yarn'"):
            svc.text_encoder_rope_kwargs(self.root / "yarn")

    @unittest.skipIf(importlib.util.find_spec("transformers") is None, "transformers is not installed in this environment")
    def test_a_real_qwen3_config_gets_the_models_base(self) -> None:
        # The defect itself: transformers 4.57 ignores `rope_parameters` and
        # falls back to 10 000; the kwarg restores the model's 1 000 000.
        from transformers import Qwen3Config

        folder = self._encoder(load_9b_config("uncensored_text_encoder"))
        fixed = Qwen3Config.from_pretrained(str(folder), local_files_only=True, **svc.text_encoder_config_kwargs(folder))
        self.assertEqual(fixed.rope_theta, 1_000_000.0)
        self.assertEqual(fixed.num_hidden_layers, svc.ENCODER_KEEP_LAYERS)
        stock = self._encoder(load_9b_config("text_encoder"), name="stock")
        self.assertEqual(Qwen3Config.from_pretrained(str(stock), local_files_only=True, **svc.text_encoder_config_kwargs(stock)).rope_theta, 1_000_000)


@unittest.skipIf(
    importlib.util.find_spec("torch") is None, "torch is not installed in this environment"
)
class TruncatedEncoderEquivalenceTests(unittest.TestCase):
    """The invariant the whole truncation rests on, checked against real torch.

    A truncated Qwen3 must produce BIT-IDENTICAL hidden states at
    `TEXT_ENCODER_OUT_LAYER_INDICES`, or every `.msprompt` ever saved becomes
    subtly wrong. A comment cannot defend that; this can. The model is random and
    tiny (hidden 32, vocab 128), so the test costs a fraction of a second.
    """

    def test_the_requested_hidden_states_survive_the_truncation(self) -> None:
        import torch
        from transformers import Qwen3Config, Qwen3Model

        keep = svc.ENCODER_KEEP_LAYERS
        full_layers = keep + 8

        def _config(layers: int) -> "Qwen3Config":
            return Qwen3Config(
                vocab_size=128,
                hidden_size=32,
                intermediate_size=64,
                num_hidden_layers=layers,
                num_attention_heads=4,
                num_key_value_heads=2,
                head_dim=8,
                max_position_embeddings=64,
                layer_types=["full_attention"] * layers,
            )

        torch.manual_seed(0)
        full = Qwen3Model(_config(full_layers)).eval()
        state = full.state_dict()

        truncated = Qwen3Model(_config(keep)).eval()
        missing, unexpected = truncated.load_state_dict(state, strict=False)
        self.assertEqual(missing, [])
        # Exactly the layers we cut away are unused, and nothing else: every
        # unexpected key names a decoder layer at or above `keep`.
        self.assertTrue(unexpected)
        for name in unexpected:
            with self.subTest(tensor=name):
                self.assertTrue(name.startswith("layers."), name)
                self.assertGreaterEqual(int(name.split(".")[1]), keep)

        ids = torch.randint(0, 128, (1, 16))
        with torch.no_grad():
            wide = full(input_ids=ids, output_hidden_states=True, use_cache=False)
            narrow = truncated(input_ids=ids, output_hidden_states=True, use_cache=False)
        for index in svc.TEXT_ENCODER_OUT_LAYER_INDICES:
            with self.subTest(layer=index):
                self.assertTrue(
                    torch.equal(wide.hidden_states[index], narrow.hidden_states[index])
                )

    def test_one_layer_fewer_would_corrupt_the_last_requested_state(self) -> None:
        # Why `ENCODER_KEEP_LAYERS` is `max(indices) + 1` and not `max(indices)`:
        # `hidden_states[num_hidden_layers]` is the `model.norm`-ed output, not a
        # raw layer output, so cutting one layer more silently changes it.
        import torch
        from transformers import Qwen3Config, Qwen3Model

        keep = svc.ENCODER_KEEP_LAYERS
        last = max(svc.TEXT_ENCODER_OUT_LAYER_INDICES)

        def _config(layers: int) -> "Qwen3Config":
            return Qwen3Config(
                vocab_size=128,
                hidden_size=32,
                intermediate_size=64,
                num_hidden_layers=layers,
                num_attention_heads=4,
                num_key_value_heads=2,
                head_dim=8,
                max_position_embeddings=64,
                layer_types=["full_attention"] * layers,
            )

        torch.manual_seed(0)
        correct = Qwen3Model(_config(keep)).eval()
        state = correct.state_dict()
        short = Qwen3Model(_config(last)).eval()
        short.load_state_dict(state, strict=False)

        ids = torch.randint(0, 128, (1, 16))
        with torch.no_grad():
            good = correct(input_ids=ids, output_hidden_states=True, use_cache=False)
            bad = short(input_ids=ids, output_hidden_states=True, use_cache=False)
        self.assertFalse(torch.equal(good.hidden_states[last], bad.hidden_states[last]))


if __name__ == "__main__":
    unittest.main()
