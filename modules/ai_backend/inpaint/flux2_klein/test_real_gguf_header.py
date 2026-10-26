"""
File: modules/ai_backend/inpaint/flux2_klein/test_real_gguf_header.py

Purpose:
Opt-in check of REAL FLUX.2 klein 4B GGUF transformer files against the model
they must load into, on `meta` only: every file tensor maps (through the BFL
conversion) onto exactly the state keys of the `Flux2Transformer2DModel` built
from the 4B package's `transformer/config.json`, every block-quantized tensor
lands on a `GgufLinear` whose `uint8` row layout is the converted rows' layout,
every quantization type is dequantizable, and no tensor byte is read.

Notes:
- Files: every `<side_models>/FLUX.2-klein-4B/transformer_gguf/*.gguf`, plus the
  files named by `MS_FLUX2_KLEIN_GGUF_FILES` (an `os.pathsep`-separated list,
  for files kept elsewhere). The model config is the one the run's probe finds
  next to the file, else the package's `FLUX.2-klein-4B/transformer/`. Without
  any file (a fresh clone) or without that config, the test skips cleanly.
- Needs torch, diffusers, accelerate and gguf; no GPU, no network, writes
  nothing.
"""

from __future__ import annotations

import os
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint.flux2_klein import components, conversion, large_model
from modules.ai_backend.runtime.paths import side_models_root

#: The 4B package directory (the catalog's `Flux2Variant::Klein4B.dir_name()`).
_PACKAGE = side_models_root() / "FLUX.2-klein-4B"

#: Environment variable naming extra GGUF files to check.
_FILES_ENV = "MS_FLUX2_KLEIN_GGUF_FILES"


def _real_files() -> list[Path]:
    """The GGUF transformer files present on this machine (module notes)."""
    found = sorted((_PACKAGE / "transformer_gguf").glob("*.gguf")) if (_PACKAGE / "transformer_gguf").is_dir() else []
    extra = [Path(item) for item in os.environ.get(_FILES_ENV, "").split(os.pathsep) if item.strip()]
    return found + [path for path in extra if path.is_file()]


def _has_stack() -> bool:
    try:
        import accelerate  # noqa: F401 - availability probe
        import diffusers  # noqa: F401 - availability probe
        import gguf  # noqa: F401 - availability probe
        import torch  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_stack(), "needs torch, diffusers, accelerate and gguf")
class RealGgufHeaderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.files = _real_files()
        if not self.files:
            self.skipTest(f"no FLUX.2 klein 4B GGUF transformer on this machine ({_PACKAGE / 'transformer_gguf'}, or set {_FILES_ENV})")

    def _config_dir(self, path: Path) -> Path:
        found = components.find_transformer_config_dir(path)
        if found is not None:
            return found
        fallback = _PACKAGE / "transformer"
        if not (fallback / "config.json").is_file():
            self.skipTest(f"{path.name}: no transformer config.json next to it and none in {fallback}")
        return fallback

    def test_every_tensor_lands_on_the_model_with_the_files_row_layout(self) -> None:
        import torch

        from modules.ai_backend.runtime.gguf_layers import GgufLinear
        from modules.ai_backend.runtime.safetensors_stream import StreamingTensorReader
        from modules.ai_backend.runtime.tensor_container import read_container_layout

        for path in self.files:
            with self.subTest(file=path.name), patch.object(StreamingTensorReader, "read_tensor", side_effect=AssertionError("a header check must read no tensor")):
                layout = read_container_layout(path)
                conversion.require_known_layout([span.name for span in layout.spans], source=path)
                large_model._require_dequantizable_gguf(layout.spans, source=path)
                options = large_model.PipelineOptions(
                    transformer_path=str(path),
                    vae_path=str(_PACKAGE / "vae"),
                    dtype="bfloat16",
                    max_sequence_length=512,
                    steps=4,
                    guidance=False,
                    text_attention_in_mask=False,
                    vae_tiling=True,
                    vae_tile_px=1024,
                    vae_scale=8,
                    transformer_format="gguf",
                )
                config_dir = self._config_dir(path)
                with patch.object(large_model, "transformer_config_dir", lambda _path: config_dir):
                    model = large_model.pipeline_spec("bfloat16").component("transformer").build_meta(options, "gguf")  # type: ignore[misc]
                # A plain meta weight is float32 until the loader stores the
                # conversion's bf16: only a quantized entry's dtype (uint8) is fixed on meta.
                expected = {key: (tuple(value.shape), value.dtype if value.dtype == torch.uint8 else None) for key, value in model.state_dict().items()}
                produced: dict[str, tuple[tuple[int, ...], object]] = {}
                for span in layout.spans:
                    placeholder = torch.empty(span.shape, dtype=getattr(torch, span.torch_dtype_name), device="meta")
                    for key, value in conversion.convert_bfl_tensor(span.name, placeholder).items() if conversion.is_bfl_key(span.name) else ((span.name, placeholder),):
                        self.assertNotIn(key, produced, f"{span.name} writes {key} twice")
                        produced[key] = (tuple(value.shape), value.dtype if span.quant_type is not None else None)
                self.assertEqual(set(produced), set(expected), "the file's tensors are exactly the model's state keys")
                for key, (shape, dtype) in produced.items():
                    self.assertEqual((shape, dtype), expected[key], key)
                quantized = large_model.gguf_quant_types(str(path))
                swapped = {f"{name}.weight" for name, module in model.named_modules() if isinstance(module, GgufLinear)}
                self.assertEqual(swapped, set(quantized))


if __name__ == "__main__":
    unittest.main()
