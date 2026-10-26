"""
File: modules/ai_backend/inpaint/flux2_klein/test_real_gguf_encoder_header.py

Purpose:
Opt-in check of REAL Qwen3 GGUF text-encoder files for FLUX.2 klein 4B against
the encoder they must load into: on `meta` only, every file tensor maps (or is
skipped by the truncation / the tied head) onto exactly the state keys of the
truncated `Qwen3Model` built from the config the run's probe finds (beside the
file, else the installed 4B package), every block-quantized tensor lands on a
`GgufLinear` / `GgufEmbedding` with the file's `uint8` row layout, every type
is dequantizable, and no tensor byte is read. A second test, for Q8_0 files
only and when the package's bf16 encoder is installed, reads a few rows of
`attn_q` / `attn_k` and checks they equal the bf16 rows within Q8_0 error under
the IDENTITY row order (llama.cpp permutes no Q/K rows for qwen3).

Notes:
- Files: every `<side_models>/FLUX.2-klein-4B/text_encoder_gguf/*.gguf`, plus the
  files named by `MS_FLUX2_KLEIN_GGUF_ENCODER_FILES` (an `os.pathsep`-separated
  list, for files kept elsewhere). Without any file (a fresh clone) or without
  a config for it, the tests skip cleanly.
- Needs torch, transformers, accelerate, diffusers and gguf; no GPU, no
  network, writes nothing.
"""

from __future__ import annotations

import json
import os
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint.flux2_klein import components, encoder_gguf, large_model

#: Environment variable naming extra GGUF encoder files to check.
_FILES_ENV = "MS_FLUX2_KLEIN_GGUF_ENCODER_FILES"

#: Rows of `attn_q` / `attn_k` compared by the row-order test, and the bound on
#: their relative error (Q8_0 measures ~0.0056; a permuted order gives ~1.4).
_ROWS = 4
_Q8_0_MAX_RELATIVE_ERROR = 0.02


def _real_files() -> list[Path]:
    """The GGUF encoder files present on this machine (module notes)."""
    folder = components.installed_4b_package_root() / "text_encoder_gguf"
    found = sorted(folder.glob("*.gguf")) if folder.is_dir() else []
    extra = [Path(item) for item in os.environ.get(_FILES_ENV, "").split(os.pathsep) if item.strip()]
    return found + [path for path in extra if path.is_file()]


def _has_stack() -> bool:
    try:
        import accelerate  # noqa: F401 - availability probe
        import diffusers  # noqa: F401 - availability probe
        import gguf  # noqa: F401 - availability probe
        import torch  # noqa: F401 - availability probe
        import transformers  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_stack(), "needs torch, transformers, accelerate, diffusers and gguf")
class RealGgufEncoderHeaderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.files = _real_files()
        if not self.files:
            self.skipTest(f"no Qwen3 GGUF text encoder on this machine ({components.installed_4b_package_root() / 'text_encoder_gguf'}, or set {_FILES_ENV})")

    def _options(self, path: Path) -> large_model.EncoderOptions:
        try:
            components.find_text_encoder_config_dir(path)
        except FileNotFoundError as exc:
            self.skipTest(f"{path.name}: no consistent text-encoder config.json ({exc})")
        return large_model.EncoderOptions(str(path), False, 512, text_encoder_format="gguf")

    def test_every_tensor_lands_on_the_model_with_the_files_row_layout(self) -> None:
        from modules.ai_backend.runtime.gguf_layers import GgufEmbedding, GgufLinear
        from modules.ai_backend.runtime.safetensors_stream import StreamingTensorReader

        keep = components.ENCODER_KEEP_LAYERS
        for path in self.files:
            with self.subTest(file=path.name), patch.object(StreamingTensorReader, "read_tensor", side_effect=AssertionError("a header check must read no tensor")):
                options = self._options(path)
                large_model.require_loadable_encoder({"text_encoder_path": str(path), "text_encoder_fp8": False})
                layout = components.gguf_layout(path)
                model = large_model.encoder_spec(False).component("text_encoder").build_meta(options, "gguf")  # type: ignore[misc]
                expected = {key: tuple(value.shape) for key, value in model.state_dict().items()}
                produced: dict[str, tuple[int, ...]] = {}
                for span in layout.spans:
                    key = encoder_gguf.encoder_state_key_of_gguf(span.name, keep)
                    if key is None:
                        continue
                    self.assertNotIn(key, produced, f"{span.name} writes {key} twice")
                    produced[key] = tuple(span.shape)
                self.assertEqual(set(produced), set(expected), "the file's kept tensors are exactly the model's state keys")
                for key, shape in produced.items():
                    self.assertEqual(shape, expected[key], key)
                quantized = encoder_gguf.gguf_encoder_quant_types(layout.spans, keep)
                swapped = {f"{name}.weight" for name, module in model.named_modules() if isinstance(module, (GgufLinear, GgufEmbedding))}
                self.assertEqual(swapped, set(quantized))

    def test_q8_0_q_k_rows_follow_the_bf16_rows_in_identity_order(self) -> None:
        import torch

        from modules.ai_backend.runtime.gguf_layers import dequantize
        from modules.ai_backend.runtime.safetensors_stream import StreamingSafetensorsReader
        from modules.ai_backend.runtime.tensor_container import open_tensor_reader

        bf16_folder = components.installed_4b_package_root() / "text_encoder"
        index_file = bf16_folder / "model.safetensors.index.json"
        if not index_file.is_file():
            self.skipTest(f"no bf16 text encoder in {bf16_folder}")
        weight_map = json.loads(index_file.read_text(encoding="utf-8"))["weight_map"]
        q8_0 = [path for path in self.files if all(span.quant_type in (None, 8) for span in components.gguf_layout(path).spans if span.name.startswith("blk.0.attn_"))]
        if not q8_0:
            self.skipTest("no Q8_0 encoder file among the checked files")
        for path in q8_0:
            with open_tensor_reader(path) as reader:
                spans = {span.name: span for span in reader.spans}
                for name in ("blk.0.attn_q.weight", "blk.0.attn_k.weight"):
                    with self.subTest(file=path.name, tensor=name):
                        hf_key = f"model.{encoder_gguf.encoder_state_key_of_gguf(name, components.ENCODER_KEEP_LAYERS)}"
                        with StreamingSafetensorsReader(bf16_folder / weight_map[hf_key]) as reference_reader:
                            reference = reference_reader.read_tensor(next(span for span in reference_reader.spans if span.name == hf_key))[:_ROWS].float()
                        rows = reader.read_tensor(spans[name])[:_ROWS]
                        values = dequantize(rows, 8, row_elements=int(reference.shape[-1])).float()
                        error = float((values - reference).norm() / reference.norm())
                        self.assertLess(error, _Q8_0_MAX_RELATIVE_ERROR, f"rows out of order or wrong (relative error {error:.4f})")
                        self.assertFalse(torch.equal(values, torch.zeros_like(values)))


if __name__ == "__main__":
    unittest.main()
