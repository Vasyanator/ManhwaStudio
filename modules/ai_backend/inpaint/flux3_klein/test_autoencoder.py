"""
File: modules/ai_backend/inpaint/flux3_klein/test_autoencoder.py

Purpose:
Unit tests for `flux3_klein/autoencoder.py` on a tiny `AeConfig`: encode/decode
shapes and dtypes, the latent BatchNorm normalize/denormalize and the 2x2
patchify being exact inverses, input refusals, `latent_ids` / `latent_shape`,
and `load_autoencoder` over a synthetic safetensors file (casts, refusals).

Notes:
Real torch on the CPU; the whole module is skipped where torch or safetensors
is absent. The synthetic checkpoint lives in a temp directory removed by the
test.
"""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

if importlib.util.find_spec("torch") is None or importlib.util.find_spec("safetensors") is None:
    raise unittest.SkipTest("torch / safetensors не установлены")

import torch
from safetensors.torch import save_file
from torch import Tensor, nn

from . import autoencoder as ae

TINY = ae.AeConfig(embed_dim=4, ch=32, ch_mult=(1, 2), num_res_blocks=1)


def _tiny_ae(seed: int = 0) -> ae.AutoEncoder:
    """A tiny AE with random weights and non-trivial BatchNorm statistics, in eval mode."""
    torch.manual_seed(seed)
    model = ae.AutoEncoder(TINY)
    model.bn.running_mean.copy_(torch.randn(TINY.latent_channels))
    model.bn.running_var.copy_(torch.rand(TINY.latent_channels) + 0.5)
    return model.eval()


class _Recorder(nn.Module):
    """Stand-in decoder: records its input and returns a zero image of the right size."""

    def __init__(self) -> None:
        super().__init__()
        self.seen: Tensor | None = None

    def forward(self, z: Tensor) -> Tensor:
        self.seen = z.clone()
        return torch.zeros(1, 3, z.shape[2] * 2, z.shape[3] * 2)


class ConfigTests(unittest.TestCase):
    def test_reference_defaults(self) -> None:
        config = ae.AeConfig()
        self.assertEqual(config.downsampling_ratio, 16)
        self.assertEqual(config.latent_channels, 128)
        self.assertEqual(ae.DOWNSAMPLING_RATIO, 16)

    def test_tiny_ratio(self) -> None:
        self.assertEqual(TINY.downsampling_ratio, 4)
        self.assertEqual(TINY.latent_channels, 16)

    def test_invalid_configs_are_refused(self) -> None:
        for kwargs in ({"ch": 48}, {"ch_mult": ()}, {"embed_dim": 0}, {"bn_eps": 0.0}, {"ch_mult": (1, 0)}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                ae.AeConfig(**kwargs)


class EncodeDecodeTests(unittest.TestCase):
    def test_encode_shapes_and_ids(self) -> None:
        model = _tiny_ae()
        tokens, ids = model.encode(torch.rand(3, 16, 24) * 2 - 1)
        self.assertEqual(tuple(tokens.shape), (4 * 6, 16))
        self.assertEqual(tokens.dtype, torch.float32)
        self.assertTrue(torch.equal(ids, ae.latent_ids(4, 6)))
        self.assertEqual(ids[7].tolist(), [0, 1, 1, 0])

    def test_decode_shape_and_dtype(self) -> None:
        model = _tiny_ae()
        image = model.decode(torch.randn(4 * 6, 16), ae.latent_ids(4, 6))
        self.assertEqual(tuple(image.shape), (3, 16, 24))
        self.assertEqual(image.dtype, torch.float32)

    def test_encode_normalizes_the_patchified_mean_with_running_stats(self) -> None:
        model = _tiny_ae()
        image = torch.rand(3, 8, 12) * 2 - 1
        tokens, _ = model.encode(image)
        with torch.no_grad():
            mean = model.encoder(image.unsqueeze(0)).chunk(2, dim=1)[0]
        h, w = mean.shape[2] // 2, mean.shape[3] // 2
        # Token (row, col), channel c*4 + dy*2 + dx = mean[c, 2row+dy, 2col+dx].
        row, col, c, dy, dx = 1, 2, 3, 1, 0
        expected = (mean[0, c, 2 * row + dy, 2 * col + dx] - model.bn.running_mean[c * 4 + dy * 2 + dx]) / torch.sqrt(
            model.bn.running_var[c * 4 + dy * 2 + dx] + TINY.bn_eps
        )
        self.assertEqual((h, w), (2, 3))
        torch.testing.assert_close(tokens[row * w + col, c * 4 + dy * 2 + dx], expected)

    def test_decode_inverts_encode_normalization_and_patchify(self) -> None:
        model = _tiny_ae()
        image = torch.rand(3, 16, 24) * 2 - 1
        tokens, ids = model.encode(image)
        with torch.no_grad():
            mean = model.encoder(image.unsqueeze(0)).chunk(2, dim=1)[0]
        recorder = _Recorder()
        model.decoder = recorder
        model.decode(tokens, ids)
        self.assertIsNotNone(recorder.seen)
        torch.testing.assert_close(recorder.seen, mean, rtol=1e-5, atol=1e-5)

    def test_bf16_autoencoder_returns_float32(self) -> None:
        model = _tiny_ae()
        model.encoder.to(torch.bfloat16)
        model.decoder.to(torch.bfloat16)
        tokens, ids = model.encode(torch.rand(3, 8, 8) * 2 - 1)
        self.assertEqual(tokens.dtype, torch.float32)
        self.assertEqual(model.decode(tokens, ids).dtype, torch.float32)

    def test_bad_inputs_are_refused(self) -> None:
        model = _tiny_ae()
        with self.assertRaises(ValueError):
            model.encode(torch.zeros(3, 10, 8))
        with self.assertRaises(ValueError):
            model.encode(torch.zeros(1, 3, 8, 8))
        with self.assertRaises(ValueError):
            model.decode(torch.zeros(5, 16), ae.latent_ids(2, 3))
        with self.assertRaises(ValueError):
            model.decode(torch.zeros(6, 8), ae.latent_ids(2, 3))


class LatentGridTests(unittest.TestCase):
    def test_latent_shape(self) -> None:
        self.assertEqual(ae.latent_shape(1024, 768), (64, 48))
        self.assertEqual(ae.latent_shape(8, 12, 4), (2, 3))

    def test_latent_shape_refuses_off_grid_sides(self) -> None:
        for height, width in ((1000, 1024), (1024, 1000), (0, 16)):
            with self.subTest(size=(height, width)), self.assertRaises(ValueError):
                ae.latent_shape(height, width)

    def test_latent_ids_row_major(self) -> None:
        ids = ae.latent_ids(2, 3)
        self.assertEqual(ids.tolist(), [[0, r, c, 0] for r in range(2) for c in range(3)])


class LoadTests(unittest.TestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.source = _tiny_ae(seed=3)
        self.state = {k: v.contiguous() for k, v in self.source.state_dict().items()}
        self.path = Path(self._dir.name) / "ae.safetensors"
        save_file(self.state, str(self.path))

    def test_bf16_load_casts_weights_but_keeps_bn_statistics(self) -> None:
        loaded = ae.load_autoencoder(self.path, device="cpu", dtype=torch.bfloat16, config=TINY)
        self.assertFalse(loaded.training)
        self.assertEqual(loaded.encoder.conv_in.weight.dtype, torch.bfloat16)
        self.assertEqual(loaded.bn.running_mean.dtype, torch.float32)
        self.assertEqual(loaded.bn.running_var.dtype, torch.float32)
        self.assertEqual(loaded.bn.num_batches_tracked.dtype, torch.int64)
        torch.testing.assert_close(loaded.bn.running_mean, self.source.bn.running_mean)
        self.assertFalse(loaded.encoder.conv_in.weight.requires_grad)

    def test_fp32_load_reproduces_the_source(self) -> None:
        loaded = ae.load_autoencoder(self.path, device="cpu", dtype=torch.float32, config=TINY)
        image = torch.rand(3, 8, 8) * 2 - 1
        expected, _ = self.source.encode(image)
        actual, _ = loaded.encode(image)
        torch.testing.assert_close(actual, expected)

    def test_missing_key_is_refused(self) -> None:
        state = dict(self.state)
        del state["decoder.conv_out.bias"]
        save_file(state, str(self.path))
        with self.assertRaises(ValueError):
            ae.load_autoencoder(self.path, device="cpu", dtype=torch.float32, config=TINY)

    def test_unsupported_dtype_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            ae.load_autoencoder(self.path, device="cpu", dtype=torch.int8, config=TINY)


if __name__ == "__main__":
    unittest.main()
