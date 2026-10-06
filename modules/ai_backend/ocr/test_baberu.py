"""
File: modules/ai_backend/ocr/test_baberu.py

Purpose:
Unit tests for the Baberu OCR backend service (`ocr/baberu.py`).

Main responsibilities:
- pin the greedy decode contract shared with the native Rust port: the
  repetition penalty over every seen id INCLUDING BOS and its sign handling,
  the content (12) and symbol (16) run caps, EOS stop, the 256-token limit,
  first-maximum tie break, step position ids from `vision_len + 1`, NaN error;
- vocab parsing / content classification / decode;
- preprocess shape, dtype, normalization and alpha drop;
- request path validation (`BaberuFiles.from_request`);
- the lease protocol: `mark_loaded` before inference, an inference failure
  keeps the model resident and evictable, a load failure leaves no entry, a
  files change rebuilds under the same key without a phantom resident, a
  provider-selection change rebuilds under `baberuocr:<provider>:<device>` and
  unloads the old key;
- session placement: vision on the selected provider (TensorRT -> CUDA),
  decoders on CPU, CPU fallback with a WARN on an unavailable provider, a
  failed build or a silent onnxruntime drop (fake `onnxruntime` module).

Notes:
The default tests need no model and no onnxruntime session: the runtime class
is replaced by a stub. The opt-in real-weight test runs only when
`MS_BABERU_MODEL_DIR` points at a downloaded `genshiai-daichi/baberu-ocr`
directory (HF layout: `onnx/`, `tokenizer/`) and skips cleanly otherwise.
"""

from __future__ import annotations

import io
import os
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import numpy as np

from modules.ai_backend.ocr import baberu as baberu_module
from modules.ai_backend.engines import paddle_onnx
from modules.ai_backend.engines.paddle_onnx import ProviderSettings
from modules.ai_backend.ocr.baberu import (
    BaberuFiles,
    BaberuOcrService,
    BaberuVocab,
    greedy_decode,
    preprocess,
    vision_provider_settings,
)
from modules.ai_backend.runtime.model_manager import LoadedModelManager
from modules.ai_backend.runtime.paths import program_root

VOCAB_SIZE = 8  # ids 0..3 special, 4..7 content/symbol under test control


def _row(**scores: float) -> np.ndarray:
    """A logits row of `VOCAB_SIZE` zeros-minus-ten with `id_<n>=score` overrides."""
    row = np.full(VOCAB_SIZE, -10.0)
    for name, score in scores.items():
        row[int(name.removeprefix("id_"))] = score
    return row


class _ScriptedSteps:
    """`step_fn` stand-in: returns scripted rows (last one repeats) and records calls."""

    def __init__(self, rows: list[np.ndarray]) -> None:
        self._rows = rows
        self.calls: list[tuple[int, int]] = []

    def __call__(self, token_id: int, position: int) -> np.ndarray:
        self.calls.append((token_id, position))
        index = min(len(self.calls) - 1, len(self._rows) - 1)
        return self._rows[index].copy()


def _never_content(_token_id: int) -> bool:
    return False


class GreedyDecodeTests(unittest.TestCase):
    def test_penalty_applies_to_bos(self) -> None:
        # BOS (id 1) wins raw (5.0) but 5.0 / 1.2 = 4.17 < 4.5, so id 4 wins.
        steps = _ScriptedSteps([_row(id_2=50.0)])
        tokens = greedy_decode(_row(id_1=5.0, id_4=4.5), steps, _never_content, first_position=257)
        self.assertEqual(tokens, [4])

    def test_penalty_multiplies_negative_scores(self) -> None:
        # After id 4 is seen, its -1.0 becomes -1.2 (multiplied), so -1.1 wins;
        # dividing instead would give -0.83 and keep id 4.
        steps = _ScriptedSteps([_row(id_4=-1.0, id_5=-1.1), _row(id_2=50.0)])
        tokens = greedy_decode(_row(id_4=3.0), steps, _never_content, first_position=257)
        self.assertEqual(tokens, [4, 5])

    def test_content_run_cap_is_twelve(self) -> None:
        row = _row(id_4=100.0, id_5=50.0, id_2=10.0)
        steps = _ScriptedSteps([row])
        tokens = greedy_decode(row, steps, lambda token_id: token_id == 4, first_position=257)
        self.assertEqual(tokens[:13], [4] * 12 + [5])

    def test_symbol_run_cap_is_sixteen(self) -> None:
        row = _row(id_4=100.0, id_5=50.0, id_2=10.0)
        steps = _ScriptedSteps([row])
        tokens = greedy_decode(row, steps, _never_content, first_position=257)
        self.assertEqual(tokens[:17], [4] * 16 + [5])

    def test_special_ids_are_never_capped(self) -> None:
        # id 3 (<unk>) repeated: `last > 3` is false, so no cap ever applies.
        row = _row(id_3=100.0, id_2=10.0)
        tokens = greedy_decode(row, _ScriptedSteps([row]), _never_content, first_position=257, max_new_tokens=40)
        self.assertEqual(tokens, [3] * 40)

    def test_eos_first_yields_empty_and_no_step(self) -> None:
        steps = _ScriptedSteps([_row()])
        self.assertEqual(greedy_decode(_row(id_2=9.0), steps, _never_content, first_position=257), [])
        self.assertEqual(steps.calls, [])

    def test_stops_at_256_tokens_without_a_trailing_step(self) -> None:
        row = _row(id_4=100.0, id_5=90.0, id_6=80.0, id_2=-50.0)
        steps = _ScriptedSteps([row])
        tokens = greedy_decode(row, steps, _never_content, first_position=257)
        self.assertEqual(len(tokens), 256)
        self.assertEqual(len(steps.calls), 255)

    def test_argmax_takes_first_maximum(self) -> None:
        steps = _ScriptedSteps([_row(id_2=50.0)])
        self.assertEqual(greedy_decode(_row(id_5=7.0, id_6=7.0), steps, _never_content, first_position=257), [5])

    def test_positions_start_at_first_position_and_feed_the_token(self) -> None:
        steps = _ScriptedSteps([_row(id_5=9.0), _row(id_6=9.0), _row(id_2=50.0)])
        tokens = greedy_decode(_row(id_4=9.0), steps, _never_content, first_position=257)
        self.assertEqual(tokens, [4, 5, 6])
        self.assertEqual(steps.calls, [(4, 257), (5, 258), (6, 259)])

    def test_nan_logits_raise(self) -> None:
        row = _row(id_4=1.0)
        row[5] = np.nan
        with self.assertRaisesRegex(RuntimeError, "NaN"):
            greedy_decode(row, _ScriptedSteps([row]), _never_content, first_position=257)


class VocabTests(unittest.TestCase):
    def test_classifies_and_decodes(self) -> None:
        vocab = BaberuVocab(["A", "1", "ー", "!", "語", "〜", "~", "ｰ"])
        self.assertEqual(vocab.vocab_size, 12)
        self.assertEqual(
            [vocab.is_content(token_id) for token_id in range(13)],
            [False] * 4 + [True, True, False, False, True, False, False, False, False],
        )
        self.assertEqual(vocab.decode([1, 4, 0, 8, 2, 3, 99, 7]), "A語!")

    def test_rejects_multi_character_entry(self) -> None:
        with self.assertRaisesRegex(ValueError, "entry 1"):
            BaberuVocab(["A", "AB"])

    def test_rejects_bad_json_and_non_list(self) -> None:
        with self.assertRaisesRegex(ValueError, "JSON"):
            BaberuVocab.from_json_bytes(b"[\"A\",")
        with self.assertRaisesRegex(ValueError, "list"):
            BaberuVocab.from_json_bytes(b"{\"a\": 1}")

    def test_from_json_bytes_reads_unicode(self) -> None:
        vocab = BaberuVocab.from_json_bytes('["私", "ド"]'.encode("utf-8"))
        self.assertEqual(vocab.decode([4, 5]), "私ド")


class PreprocessTests(unittest.TestCase):
    def test_shape_dtype_and_white_normalization(self) -> None:
        from PIL import Image

        tensor = preprocess(Image.new("RGB", (50, 30), (255, 255, 255)))
        self.assertEqual(tensor.shape, (1, 3, 224, 224))
        self.assertEqual(tensor.dtype, np.float32)
        self.assertTrue(tensor.flags["C_CONTIGUOUS"])
        expected = (1.0 - np.array([0.485, 0.456, 0.406])) / np.array([0.229, 0.224, 0.225])
        np.testing.assert_allclose(tensor[0, :, 100, 100], expected, rtol=1e-6)

    def test_alpha_is_dropped_not_composited(self) -> None:
        from PIL import Image

        rgba = preprocess(Image.new("RGBA", (16, 16), (10, 20, 30, 0)))
        rgb = preprocess(Image.new("RGB", (16, 16), (10, 20, 30)))
        np.testing.assert_array_equal(rgba, rgb)


class BaberuFilesTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms_baberu_files_")
        self.root = Path(self._tmp.name)
        self.paths: dict[str, str] = {}
        for name in ("vision", "prefill", "step", "vocab"):
            path = self.root / f"{name}.bin"
            path.write_bytes(b"x")
            self.paths[name] = str(path)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_valid_paths(self) -> None:
        files = BaberuFiles.from_request(self.paths)
        self.assertEqual(files.step, Path(self.paths["step"]))

    def test_missing_entry_is_named(self) -> None:
        del self.paths["vocab"]
        with self.assertRaisesRegex(ValueError, "'vocab'"):
            BaberuFiles.from_request(self.paths)

    def test_relative_path_is_rejected(self) -> None:
        self.paths["prefill"] = "onnx/decoder_prefill_int8.onnx"
        with self.assertRaisesRegex(ValueError, "'prefill' must be an absolute path"):
            BaberuFiles.from_request(self.paths)

    def test_missing_file_names_the_path(self) -> None:
        missing = str(self.root / "absent.onnx")
        self.paths["vision"] = missing
        with self.assertRaisesRegex(ValueError, "does not exist"):
            BaberuFiles.from_request(self.paths)


def _png_bytes() -> bytes:
    from PIL import Image

    buffer = io.BytesIO()
    Image.new("RGB", (40, 20), (255, 255, 255)).save(buffer, format="PNG")
    return buffer.getvalue()


class _FakeDeviceService:
    """`AiDeviceService.get_state` stand-in with a mutable ONNX selection."""

    def __init__(self, provider: str = "CPUExecutionProvider", device_id: str = "0") -> None:
        self.provider = provider
        self.device_id = device_id

    def get_state(self) -> dict[str, Any]:
        return {"selected_onnx_provider": self.provider, "selected_onnx_device_id": self.device_id}


class _StubRuntime:
    """Replaces `_BaberuOnnxRuntime`: no ORT; behaviour driven by class attributes."""

    fail_load = False
    fail_inference = False
    on_recognize: Any = None
    loads: list[tuple[BaberuFiles, ProviderSettings]] = []

    def __init__(self, files: BaberuFiles, settings: ProviderSettings) -> None:
        if _StubRuntime.fail_load:
            raise RuntimeError("stub load failure")
        self.files = files
        self.vision_device = f"{settings.provider}:{settings.device_id}"
        _StubRuntime.loads.append((files, settings))

    def recognize(self, _image: Any) -> str:
        if _StubRuntime.on_recognize is not None:
            _StubRuntime.on_recognize()
        if _StubRuntime.fail_inference:
            raise RuntimeError("stub inference failure")
        return " first \nsecond"


class LeaseProtocolTests(unittest.TestCase):
    def setUp(self) -> None:
        _StubRuntime.fail_load = False
        _StubRuntime.fail_inference = False
        _StubRuntime.on_recognize = None
        _StubRuntime.loads = []
        patcher = mock.patch.object(baberu_module, "_BaberuOnnxRuntime", _StubRuntime)
        patcher.start()
        self.addCleanup(patcher.stop)
        self._tmp = tempfile.TemporaryDirectory(prefix="ms_baberu_lease_")
        self.addCleanup(self._tmp.cleanup)
        self.manager = LoadedModelManager()
        self.devices = _FakeDeviceService()
        self.service = BaberuOcrService(self.manager, self.devices)

    def _files(self, tag: str) -> dict[str, str]:
        paths: dict[str, str] = {}
        for name in ("vision", "prefill", "step", "vocab"):
            path = Path(self._tmp.name) / f"{tag}_{name}.bin"
            path.write_bytes(b"x")
            paths[name] = str(path)
        return paths

    def _resident(self) -> int:
        return self.manager.health()["resident_model_count"]

    def test_success_formats_lines_and_marks_loaded_before_inference(self) -> None:
        seen: list[dict[str, int]] = []
        _StubRuntime.on_recognize = lambda: seen.append(self.manager.health())
        result = self.service.recognize_image_bytes(_png_bytes(), model_files=self._files("a"), reflect_strings=True)
        self.assertEqual(result, {"lines": ["second", "first"], "text": "second\nfirst"})
        self.assertEqual(seen[0]["resident_model_count"], 1)
        self.assertEqual(seen[0]["loading_model_count"], 0)
        self.assertEqual(
            self.service.health(),
            {"ready": True, "device": "CPUExecutionProvider:0", "last_error": None},
        )
        self.assertIn("baberuocr:CPUExecutionProvider:0", self.manager._entries)

    def test_inference_failure_keeps_the_model_counted_and_evictable(self) -> None:
        _StubRuntime.fail_inference = True
        with self.assertRaisesRegex(RuntimeError, "inference failed"):
            self.service.recognize_image_bytes(_png_bytes(), model_files=self._files("a"))
        self.assertEqual(self._resident(), 1)
        self.assertIn("stub inference failure", self.service.health()["last_error"])
        # The callback the manager would use for eviction still owns the model.
        key = "baberuocr:CPUExecutionProvider:0"
        self.assertTrue(self.service._unload_key(key))
        self.assertEqual(self._resident(), 0)
        self.assertFalse(self.service._unload_key(key))

    def test_load_failure_leaves_no_entry(self) -> None:
        _StubRuntime.fail_load = True
        with self.assertRaisesRegex(RuntimeError, "init failed"):
            self.service.recognize_image_bytes(_png_bytes(), model_files=self._files("a"))
        self.assertEqual(self.manager.health()["resident_model_count"], 0)
        self.assertEqual(self.manager._entries, {})
        self.assertFalse(self.service.health()["ready"])

    def test_same_files_reuse_and_changed_files_rebuild_under_one_key(self) -> None:
        files_a, files_b = self._files("a"), self._files("b")
        self.service.recognize_image_bytes(_png_bytes(), model_files=files_a)
        self.service.recognize_image_bytes(_png_bytes(), model_files=files_a)
        self.assertEqual(len(_StubRuntime.loads), 1)
        self.service.recognize_image_bytes(_png_bytes(), model_files=files_b)
        self.assertEqual(len(_StubRuntime.loads), 2)
        self.assertEqual(_StubRuntime.loads[1][0].vision, Path(files_b["vision"]))
        self.assertEqual(self._resident(), 1)

    def test_selection_change_rebuilds_under_the_new_key_and_unloads_the_old(self) -> None:
        files = self._files("a")
        self.service.recognize_image_bytes(_png_bytes(), model_files=files)
        self.devices.provider, self.devices.device_id = "CUDAExecutionProvider", "1"
        self.service.recognize_image_bytes(_png_bytes(), model_files=files)
        self.assertEqual(len(_StubRuntime.loads), 2)
        self.assertEqual(_StubRuntime.loads[1][1], ProviderSettings("CUDAExecutionProvider", "1"))
        self.assertEqual(self._resident(), 1)
        self.assertEqual(set(self.manager._entries), {"baberuocr:CUDAExecutionProvider:1"})
        self.assertEqual(self.service.health()["device"], "CUDAExecutionProvider:1")
        # The old key's eviction callback no longer owns anything.
        self.assertFalse(self.service._unload_key("baberuocr:CPUExecutionProvider:0"))
        self.assertTrue(self.service._unload_key("baberuocr:CUDAExecutionProvider:1"))
        self.assertEqual(self._resident(), 0)
        self.assertIsNone(self.service.health()["device"])

    def test_failed_rebuild_under_a_resident_key_leaves_no_phantom(self) -> None:
        self.service.recognize_image_bytes(_png_bytes(), model_files=self._files("a"))
        self.assertEqual(self._resident(), 1)
        _StubRuntime.fail_load = True
        with self.assertRaises(RuntimeError):
            self.service.recognize_image_bytes(_png_bytes(), model_files=self._files("b"))
        self.assertEqual(self._resident(), 0)
        self.assertEqual(self.manager._entries, {})
        self.assertFalse(self.service.health()["ready"])

    def test_invalid_paths_never_take_a_lease(self) -> None:
        with self.assertRaises(ValueError):
            self.service.recognize_image_bytes(_png_bytes(), model_files={"vision": "rel"})
        self.assertEqual(self.manager._entries, {})


class _FakeNode:
    def __init__(self, name: str) -> None:
        self.name = name


# Every graph name of the Baberu contract, so one fake session passes `_require_io` for all three.
_ALL_IO = [
    "pixel_values", "vision_embeds", "input_ids", "position_ids", "logits",
    *baberu_module.PRESENT_TO_PAST, *baberu_module.PRESENT_TO_PAST.values(),
]


class _FakeSession:
    def __init__(self, providers: list[str]) -> None:
        self._providers = providers

    def get_providers(self) -> list[str]:
        return list(self._providers)

    def get_inputs(self) -> list[_FakeNode]:
        return [_FakeNode(name) for name in _ALL_IO]

    def get_outputs(self) -> list[_FakeNode]:
        return [_FakeNode(name) for name in _ALL_IO]


class _FakeOrt:
    """Minimal `onnxruntime` stand-in: records `providers=` per session build.

    `fail` lists provider names whose build raises; `silent_drop` lists
    provider names onnxruntime "accepts" but runs on the CPU.
    """

    class GraphOptimizationLevel:
        ORT_ENABLE_ALL = "all"

    class SessionOptions:
        graph_optimization_level: Any = None

    def __init__(self, fail: tuple[str, ...] = (), silent_drop: tuple[str, ...] = ()) -> None:
        self.fail = fail
        self.silent_drop = silent_drop
        self.builds: list[list[Any]] = []

    def InferenceSession(self, _path: str, sess_options: Any, providers: list[Any]) -> _FakeSession:  # noqa: N802
        self.builds.append(list(providers))
        first = providers[0][0] if isinstance(providers[0], tuple) else providers[0]
        if first in self.fail:
            raise RuntimeError(f"{first} build failed")
        if first in self.silent_drop:
            return _FakeSession(["CPUExecutionProvider"])
        return _FakeSession([first, "CPUExecutionProvider"] if first != "CPUExecutionProvider" else [first])


class VisionPlacementTests(unittest.TestCase):
    AVAILABLE = ["CUDAExecutionProvider", "DmlExecutionProvider", "CPUExecutionProvider"]

    def setUp(self) -> None:
        # `provider_attempts` consults the real onnxruntime's available providers.
        def attempts(settings: ProviderSettings) -> list[list[Any]]:
            if settings.provider not in self.AVAILABLE:
                return []
            return [[paddle_onnx.provider_spec(settings.provider, settings)]]

        patcher = mock.patch.object(paddle_onnx, "provider_attempts", attempts)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.path = Path("/models/vision_fp16.onnx")

    def _build(self, ort: _FakeOrt, provider: str, device_id: str = "0") -> tuple[Any, str]:
        return baberu_module._build_vision_session(ort, self.path, ProviderSettings(provider, device_id))

    def test_tensorrt_maps_to_cuda_on_the_same_device_and_others_pass_through(self) -> None:
        self.assertEqual(
            vision_provider_settings(ProviderSettings("TensorrtExecutionProvider", "2")),
            ProviderSettings("CUDAExecutionProvider", "2"),
        )
        for provider in ("CPUExecutionProvider", "CUDAExecutionProvider", "DmlExecutionProvider"):
            self.assertEqual(vision_provider_settings(ProviderSettings(provider, "1")), ProviderSettings(provider, "1"))

    def test_vision_on_the_selected_gpu(self) -> None:
        ort = _FakeOrt()
        _session, device = self._build(ort, "CUDAExecutionProvider", "1")
        self.assertEqual(device, "CUDAExecutionProvider:1")
        self.assertEqual(ort.builds, [[("CUDAExecutionProvider", {"device_id": "1"})]])

    def test_tensorrt_selection_builds_vision_on_cuda(self) -> None:
        ort = _FakeOrt()
        _session, device = self._build(ort, "TensorrtExecutionProvider")
        self.assertEqual(device, "CUDAExecutionProvider:0")
        self.assertEqual(ort.builds, [[("CUDAExecutionProvider", {"device_id": "0"})]])

    def test_cpu_selection_builds_only_on_cpu(self) -> None:
        ort = _FakeOrt()
        _session, device = self._build(ort, "CPUExecutionProvider")
        self.assertEqual(device, "CPUExecutionProvider")
        self.assertEqual(ort.builds, [["CPUExecutionProvider"]])

    def test_failed_build_falls_back_to_cpu_with_a_warning(self) -> None:
        ort = _FakeOrt(fail=("DmlExecutionProvider",))
        with self.assertLogs(baberu_module.log, "WARNING") as logs:
            _session, device = self._build(ort, "DmlExecutionProvider")
        self.assertEqual(device, "CPUExecutionProvider")
        self.assertEqual(ort.builds[-1], ["CPUExecutionProvider"])
        self.assertIn("effective provider=CPUExecutionProvider", logs.output[0])

    def test_silent_onnxruntime_drop_is_reported_and_reports_cpu(self) -> None:
        ort = _FakeOrt(silent_drop=("CUDAExecutionProvider",))
        with self.assertLogs(baberu_module.log, "WARNING") as logs:
            _session, device = self._build(ort, "CUDAExecutionProvider")
        self.assertEqual(device, "CPUExecutionProvider")
        self.assertEqual(len(ort.builds), 1)
        self.assertIn("silently", logs.output[0])

    def test_unavailable_provider_falls_back_to_cpu(self) -> None:
        ort = _FakeOrt()
        with self.assertLogs(baberu_module.log, "WARNING"):
            _session, device = self._build(ort, "OpenVINOExecutionProvider")
        self.assertEqual(device, "CPUExecutionProvider")
        self.assertEqual(ort.builds, [["CPUExecutionProvider"]])

    def test_cpu_failure_raises(self) -> None:
        ort = _FakeOrt(fail=("CUDAExecutionProvider", "CPUExecutionProvider"))
        with self.assertLogs(baberu_module.log, "WARNING"), self.assertRaisesRegex(RuntimeError, "Baberu ONNX session"):
            self._build(ort, "CUDAExecutionProvider")

    def test_runtime_puts_vision_on_the_gpu_and_both_decoders_on_cpu(self) -> None:
        ort = _FakeOrt()
        with tempfile.TemporaryDirectory(prefix="ms_baberu_place_") as tmp:
            vocab = Path(tmp) / "vocab.json"
            vocab.write_text('["A"]', encoding="utf-8")
            files = BaberuFiles(Path(tmp) / "v.onnx", Path(tmp) / "p.onnx", Path(tmp) / "s.onnx", vocab)
            with mock.patch.dict("sys.modules", {"onnxruntime": ort}):
                runtime = baberu_module._BaberuOnnxRuntime(files, ProviderSettings("DmlExecutionProvider", "1"))
        self.assertEqual(runtime.vision_device, "DmlExecutionProvider:1")
        self.assertEqual(
            ort.builds,
            [[("DmlExecutionProvider", {"device_id": "1"})], ["CPUExecutionProvider"], ["CPUExecutionProvider"]],
        )


@unittest.skipUnless(os.environ.get("MS_BABERU_MODEL_DIR"), "set MS_BABERU_MODEL_DIR to run the real-weight Baberu check")
class RealWeightsTests(unittest.TestCase):
    """Opt-in: the real ONNX export reads a rendered English bubble exactly."""

    def test_reads_rendered_english_bubble(self) -> None:
        from PIL import Image, ImageDraw, ImageFont

        model_dir = Path(os.environ["MS_BABERU_MODEL_DIR"]).resolve()
        files = {
            "vision": str(model_dir / "onnx" / "vision_fp16.onnx"),
            "prefill": str(model_dir / "onnx" / "decoder_prefill_int8.onnx"),
            "step": str(model_dir / "onnx" / "decoder_step_int8.onnx"),
            "vocab": str(model_dir / "tokenizer" / "vocab.json"),
        }
        font = ImageFont.truetype(str(program_root() / "fonts" / "ui" / "bold" / "00-NotoSans-Bold.ttf"), 36)
        lines = ["I WON'T LET YOU", "GET AWAY WITH THIS!"]
        width = max(int(font.getlength(line)) for line in lines) + 40
        image = Image.new("RGB", (width, len(lines) * 46 + 40), "white")
        draw = ImageDraw.Draw(image)
        for index, line in enumerate(lines):
            draw.text((20, 20 + index * 46), line, font=font, fill="black")
        buffer = io.BytesIO()
        image.save(buffer, format="PNG")

        service = BaberuOcrService(LoadedModelManager(), _FakeDeviceService())
        result = service.recognize_image_bytes(buffer.getvalue(), model_files=files)

        self.assertEqual(result["lines"], lines)


if __name__ == "__main__":
    unittest.main()
