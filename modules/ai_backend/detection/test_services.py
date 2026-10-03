"""
File: modules/ai_backend/detection/test_services.py

Purpose:
Unit tests for what each forward-only detector service feeds its network and
which output it keeps, without any real weights.

Main responsibilities:
- CTD: the net receives RGB `/255` NCHW float32 and the service returns
  `[seg, lines[:, 0]]` (a fake net with distinguishable channels catches a
  switch to the DB threshold map `lines[:, 1]`);
- Paddle: `normalize_det_rgb` matches the ImageNet formula, OCR's
  `preprocess_det_image` shares it, and `forward_tiles` feeds exactly that
  tensor to the ONNX runner (with the MIGraphX->CPU detection rule);
- Surya: the model receives the predictor processor's output untouched (no
  resize), channel 0 is returned at a quarter of the tile size, and the CUDA
  cache is released after the forward when CUDA is available.

Notes:
The seams are the existing ones: `CtdTextDetectorService._forward_locked` takes
any callable net, `surya._forward_with_predictor` any object with `.processor`
and `.model`, and `PaddleTextDetectorService` takes its runtime factory. The
Torch-based tests skip when Torch is not installed; the Surya tests also need
the light `surya.settings` module (its `INFERENCE_MODE`). Nothing is written
to disk.
"""

from __future__ import annotations

import sys
from pathlib import Path
from types import SimpleNamespace

import numpy as np
import pytest

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/detection/` -> parents[2] = program root, where `config`
# and `modules.ai_backend...` resolve from.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

from modules.ai_backend.engines import paddle_onnx  # noqa: E402
from modules.ai_backend.engines.paddle_onnx import (  # noqa: E402
    ProviderSettings,
    normalize_det_rgb,
    parse_det_config,
    preprocess_det_image,
)
from modules.ai_backend.detection import paddle as paddle_detector  # noqa: E402

# ImageNet constants the PP-OCR detector was trained with, stated independently
# of `parse_det_config` so a drift there fails here.
IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)


def _patterned_tiles(n: int, height: int, width: int) -> np.ndarray:
    """RGB tiles whose three channels hold different values everywhere."""
    base = np.arange(n * height * width, dtype=np.int64).reshape(n, height, width)
    tiles = np.stack([base % 251, (base * 7 + 3) % 253, (base * 13 + 101) % 256], axis=-1)
    return tiles.astype(np.uint8)


# --- Paddle -----------------------------------------------------------------


def test_normalize_det_rgb_matches_imagenet_formula() -> None:
    pixel = np.array([[[[255, 0, 128]]]], dtype=np.uint8)  # one RGB pixel, [1, 1, 1, 3]
    out = normalize_det_rgb(pixel, parse_det_config(None))
    assert out.dtype == np.float32
    assert out.shape == (1, 3, 1, 1)
    assert out.flags["C_CONTIGUOUS"]
    expected = [(v / 255.0 - m) / s for v, m, s in zip((255, 0, 128), IMAGENET_MEAN, IMAGENET_STD)]
    np.testing.assert_allclose(out.ravel(), expected, rtol=0, atol=1e-6)


def test_ocr_preprocess_shares_detection_normalization() -> None:
    # 64x96 needs no resize (multiple of the stride, below `resize_long`), so
    # OCR's pass must equal the forward-only normalization of the RGB image.
    cfg = parse_det_config(None)
    rgb = _patterned_tiles(1, 64, 96)[0]
    bgr = np.ascontiguousarray(rgb[:, :, ::-1])
    batch, src_h, src_w = preprocess_det_image(bgr, cfg)
    assert (src_h, src_w) == (64, 96)
    np.testing.assert_array_equal(batch, normalize_det_rgb(rgb[np.newaxis], cfg))


class _FakeDetRunner:
    """Records the detector input and returns a fixed probability batch."""

    def __init__(self, prob: np.ndarray) -> None:
        self.prob = prob
        self.selected_provider = "CPUExecutionProvider"
        self.inputs: list[np.ndarray] = []

    def run(self, det_input: np.ndarray) -> np.ndarray:
        self.inputs.append(det_input.copy())
        return self.prob


class _FakeRuntimeFactory:
    """Duck-typed `RuntimeFactory`: hands out one fake runner and records leases."""

    def __init__(self, runner: _FakeDetRunner) -> None:
        self.runner = runner
        self.acquired: list[tuple[Path, ProviderSettings]] = []
        self.released = 0

    def acquire_runner(self, model_path: Path, settings: ProviderSettings):
        self.acquired.append((model_path, settings))
        return SimpleNamespace(runner=self.runner, release=self._release)

    def _release(self) -> None:
        self.released += 1


def test_paddle_forward_feeds_normalized_tiles_and_quantizes(monkeypatch, tmp_path) -> None:
    n, height, width = 2, 32, 64
    tiles = _patterned_tiles(n, height, width)
    prob = np.full((n, 1, height, width), 0.25, dtype=np.float32)
    prob[1] = 0.75
    runner = _FakeDetRunner(prob)
    factory = _FakeRuntimeFactory(runner)
    # No `config.json` beside the fake model path, so the defaults apply.
    monkeypatch.setattr(paddle_onnx, "resolve_det_model_path", lambda: tmp_path / "det.onnx")
    monkeypatch.setattr(
        paddle_detector,
        "resolve_provider_settings",
        lambda _cfg: ProviderSettings(provider="MIGraphXExecutionProvider", device_id="1"),
    )

    maps = paddle_detector.PaddleTextDetectorService(factory).forward_tiles(tiles)

    assert len(runner.inputs) == 1
    np.testing.assert_array_equal(runner.inputs[0], normalize_det_rgb(tiles, parse_det_config(None)))
    # Detection never runs on MIGraphX: the session is requested on the CPU.
    assert [settings for _path, settings in factory.acquired] == [
        ProviderSettings(provider="CPUExecutionProvider", device_id="0")
    ]
    assert factory.released == 1
    assert maps.dtype == np.uint8
    assert maps.shape == (n, 1, height, width)
    assert set(maps[0].ravel().tolist()) == {64}  # floor(0.25 * 255 + 0.5)
    assert set(maps[1].ravel().tolist()) == {191}  # floor(0.75 * 255 + 0.5)


def test_paddle_forward_rejects_non_finite_output(monkeypatch, tmp_path) -> None:
    prob = np.zeros((1, 1, 32, 32), dtype=np.float32)
    prob[0, 0, 0, 0] = np.nan
    factory = _FakeRuntimeFactory(_FakeDetRunner(prob))
    monkeypatch.setattr(paddle_onnx, "resolve_det_model_path", lambda: tmp_path / "det.onnx")
    monkeypatch.setattr(
        paddle_detector,
        "resolve_provider_settings",
        lambda _cfg: ProviderSettings(provider="CPUExecutionProvider"),
    )
    with pytest.raises(RuntimeError, match="non-finite"):
        paddle_detector.PaddleTextDetectorService(factory).forward_tiles(_patterned_tiles(1, 32, 32))
    assert factory.released == 1


# --- CTD --------------------------------------------------------------------


class _FakeCtdNet:
    """Records its input; returns `(blocks, seg, lines)` like `TextDetBase`.

    `seg` is 0.5, `lines[:, 0]` (shrink) 0.25 and `lines[:, 1]` (threshold)
    0.75, so every channel quantizes to a different level (128 / 64 / 191).
    """

    def __init__(self, torch_module, *, lines_channels: int = 2, seg_value: float = 0.5) -> None:
        self.torch = torch_module
        self.lines_channels = lines_channels
        self.seg_value = seg_value
        self.inputs: list = []

    def __call__(self, batch):
        self.inputs.append(batch.detach().clone())
        n, _c, height, width = batch.shape
        seg = self.torch.full((n, 1, height, width), self.seg_value, dtype=self.torch.float32)
        lines = self.torch.empty((n, self.lines_channels, height, width), dtype=self.torch.float32)
        lines[:, 0] = 0.25
        lines[:, 1:] = 0.75
        return [], seg, lines


def test_ctd_feeds_rgb_over_255_nchw_and_keeps_seg_and_shrink() -> None:
    torch = pytest.importorskip("torch")
    from modules.ai_backend.detection.ctd import CtdTextDetectorService

    tiles = _patterned_tiles(2, 64, 128)
    net = _FakeCtdNet(torch)
    maps = CtdTextDetectorService._forward_locked(net, tiles, "cpu")

    assert len(net.inputs) == 1
    received = net.inputs[0]
    assert received.dtype == torch.float32
    assert received.device.type == "cpu"
    expected = tiles.transpose(0, 3, 1, 2).astype(np.float32) / np.float32(255.0)
    np.testing.assert_array_equal(received.numpy(), expected)

    assert maps.dtype == np.uint8
    assert maps.shape == (2, 2, 64, 128)
    assert set(maps[:, 0].ravel().tolist()) == {128}  # seg
    assert set(maps[:, 1].ravel().tolist()) == {64}  # lines[:, 0], not the threshold map


def test_ctd_rejects_unexpected_output_shape() -> None:
    torch = pytest.importorskip("torch")
    from modules.ai_backend.detection.ctd import CtdTextDetectorService

    class _HalfSizeNet(_FakeCtdNet):
        def __call__(self, batch):
            _blocks, seg, lines = super().__call__(batch)
            return [], seg[:, :, ::2], lines

    with pytest.raises(RuntimeError, match="segmentation output has shape"):
        CtdTextDetectorService._forward_locked(_HalfSizeNet(torch), _patterned_tiles(1, 64, 64), "cpu")


def test_ctd_rejects_non_finite_output() -> None:
    torch = pytest.importorskip("torch")
    from modules.ai_backend.detection.ctd import CtdTextDetectorService

    net = _FakeCtdNet(torch, seg_value=float("inf"))
    with pytest.raises(RuntimeError, match="non-finite"):
        CtdTextDetectorService._forward_locked(net, _patterned_tiles(1, 64, 64), "cpu")


# --- Surya ------------------------------------------------------------------


def _fake_surya_processor(received: list[np.ndarray]):
    """Stand-in for the library processor: records each tile, returns a CHW map.

    The output is a deliberate, recognisable transform (not the real ImageNet
    one), so the test proves the service passes it to the model untouched.
    """

    def process(tile: np.ndarray) -> dict:
        received.append(tile.copy())
        chw = tile.transpose(2, 0, 1).astype(np.float32) / np.float32(255.0) - np.float32(0.5)
        return {"pixel_values": [chw]}

    return process


class _FakeSuryaModel:
    """Two-channel segformer stand-in at the native quarter resolution."""

    def __init__(self, torch_module) -> None:
        self.torch = torch_module
        self.dtype = torch_module.float32
        self.device = torch_module.device("cpu")
        self.inputs: list = []

    def __call__(self, *, pixel_values):
        self.inputs.append(pixel_values.detach().clone())
        n, _c, height, width = pixel_values.shape
        logits = self.torch.empty((n, 2, height // 4, width // 4), dtype=self.torch.float32)
        logits[:, 0] = 0.25  # text heatmap
        logits[:, 1] = 0.75  # second head, never returned
        return SimpleNamespace(logits=logits)


def _surya_module():
    pytest.importorskip("torch")
    pytest.importorskip("surya.settings")
    from modules.ai_backend.detection import surya as surya_detector

    return surya_detector


def test_surya_feeds_processor_output_and_returns_channel_zero_at_quarter_size() -> None:
    torch = pytest.importorskip("torch")
    surya_detector = _surya_module()

    tiles = _patterned_tiles(2, 32, 48)
    received: list[np.ndarray] = []
    model = _FakeSuryaModel(torch)
    predictor = SimpleNamespace(processor=_fake_surya_processor(received), model=model)

    maps = surya_detector._forward_with_predictor(predictor, tiles)

    # The processor saw every tile at its own size: no resize before it.
    assert len(received) == 2
    for got, tile in zip(received, tiles):
        np.testing.assert_array_equal(got, tile)
    assert len(model.inputs) == 1
    fed = model.inputs[0]
    assert fed.dtype == torch.float32
    assert tuple(fed.shape) == (2, 3, 32, 48)
    expected = tiles.transpose(0, 3, 1, 2).astype(np.float32) / np.float32(255.0) - np.float32(0.5)
    np.testing.assert_array_equal(fed.numpy(), expected)

    assert maps.dtype == np.uint8
    assert maps.shape == (2, 1, 8, 12)
    assert set(maps.ravel().tolist()) == {64}  # channel 0 (0.25), not channel 1 (0.75)


def test_surya_rejects_upsampled_output() -> None:
    torch = pytest.importorskip("torch")
    surya_detector = _surya_module()

    class _FullSizeModel(_FakeSuryaModel):
        def __call__(self, *, pixel_values):
            n, _c, height, width = pixel_values.shape
            return SimpleNamespace(logits=self.torch.zeros((n, 2, height, width)))

    predictor = SimpleNamespace(processor=_fake_surya_processor([]), model=_FullSizeModel(torch))
    with pytest.raises(RuntimeError, match="output has shape"):
        surya_detector._forward_with_predictor(predictor, _patterned_tiles(1, 32, 32))


@pytest.mark.parametrize("cuda_available", [True, False])
def test_surya_releases_cuda_cache_only_when_cuda_is_available(monkeypatch, cuda_available: bool) -> None:
    torch = pytest.importorskip("torch")
    surya_detector = _surya_module()

    calls: list[str] = []
    monkeypatch.setattr(torch.cuda, "is_available", lambda: cuda_available)
    monkeypatch.setattr(torch.cuda, "empty_cache", lambda: calls.append("empty_cache"))
    predictor = SimpleNamespace(processor=_fake_surya_processor([]), model=_FakeSuryaModel(torch))

    surya_detector._forward_with_predictor(predictor, _patterned_tiles(1, 32, 32))

    assert calls == (["empty_cache"] if cuda_available else [])
