"""
FILE OVERVIEW: modules/ai_backend/manga_ocr_service.py
Hybrid MangaOCR service with ONNX and optional PyTorch backends.

Main responsibilities:
- Resolve local MangaOCR ONNX weights from `ManhwaStudio_AI_Models/ONNX/MangaOCR/*`.
- Build and reuse encoder/decoder ONNX Runtime sessions for the selected provider.
- Lazily load the original `manga_ocr` PyTorch package only when the PyTorch variant is selected.
- Keep MangaOCR preprocessing and text postprocessing compatible with the original package.
- Integrate with the shared loaded-model manager used by the Python AI backend.

Key structures:
- `MangaOcrService`
- `_OnnxMangaOcrRuntime`
- `_TorchMangaOcrRuntime`
- `_BeamCandidate`

Notes:
- ONNX weights are loaded exclusively through `onnxruntime`.
- Absence of `manga_ocr` must not break ONNX variants.
"""

from __future__ import annotations

import gc
import io
import logging
import math
import re
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import jaconv
import numpy as np

from .device_service import AiDeviceService
from .model_manager import LoadedModelManager
from .paddle_onnx_runtime import (
    ProviderSettings,
    _configure_onnx_cache_environment,
    provider_attempts,
    resolve_compiled_cache_root,
)

try:
    import onnxruntime as ort  # type: ignore
except Exception as exc:  # pragma: no cover - environment specific
    ort = None
    ORT_IMPORT_ERROR: Exception | None = exc
else:
    ORT_IMPORT_ERROR = None


log = logging.getLogger(__name__)

MODELS_DIR_NAME = "ManhwaStudio_AI_Models"
ONNX_DIR_NAME = "ONNX"
MANGA_OCR_MODEL_DIRS: dict[str, str] = {
    "base_onnx": "base",
    "2025_onnx": "2025",
}
ENCODER_FILE_NAME = "encoder_model.onnx"
DECODER_FILE_NAME = "decoder_model.onnx"


def _clear_runtime_cache() -> None:
    try:
        import torch  # type: ignore
    except Exception:
        gc.collect()
        return

    gc.collect()
    try:
        if hasattr(torch, "cuda") and torch.cuda.is_available():
            torch.cuda.empty_cache()
            torch.cuda.ipc_collect()
    except Exception:
        pass
    try:
        if hasattr(torch, "mps") and hasattr(torch.mps, "empty_cache"):
            torch.mps.empty_cache()
    except Exception:
        pass


@dataclass(frozen=True)
class _BeamCandidate:
    token_ids: tuple[int, ...]
    sum_logprob: float
    finished: bool

    def normalized_score(self, length_penalty: float) -> float:
        effective_len = max(len(self.token_ids) - 1, 1)
        return self.sum_logprob / math.pow(float(effective_len), length_penalty)


@dataclass(frozen=True)
class _LeaseContext:
    lease: Any
    model_key: str
    settings: ProviderSettings
    needs_load: bool

    def mark_loaded(self, unload_callback) -> None:
        self.lease.mark_loaded(unload_callback=unload_callback)

    def mark_load_failed(self) -> None:
        self.lease.mark_load_failed()

    def release(self) -> None:
        self.lease.release()


class _OnnxMangaOcrRuntime:
    def __init__(self, model_dir: Path, settings: ProviderSettings) -> None:
        if ort is None:
            raise RuntimeError(f"onnxruntime import failed: {ORT_IMPORT_ERROR}")

        self._model_dir = model_dir
        self._settings = settings
        self._encoder_lock = threading.Lock()
        self._decoder_lock = threading.Lock()
        self._encoder_session = self._build_session(model_dir / ENCODER_FILE_NAME, settings)
        self._decoder_session = self._build_session(model_dir / DECODER_FILE_NAME, settings)
        self._encoder_input_name = self._encoder_session.get_inputs()[0].name
        self._encoder_output_name = self._encoder_session.get_outputs()[0].name
        self._decoder_input_ids_name = self._decoder_session.get_inputs()[0].name
        self._decoder_encoder_states_name = self._decoder_session.get_inputs()[1].name
        self._decoder_output_name = self._decoder_session.get_outputs()[0].name

        try:
            from transformers import AutoTokenizer, GenerationConfig, ViTImageProcessor
        except Exception as exc:
            raise RuntimeError(f"transformers import failed: {exc}") from exc

        try:
            self.processor = ViTImageProcessor.from_pretrained(model_dir, local_files_only=True)
            self.tokenizer = AutoTokenizer.from_pretrained(
                self._resolve_tokenizer_dir(model_dir),
                local_files_only=True,
            )
            self.generation_config = GenerationConfig.from_pretrained(
                model_dir,
                local_files_only=True,
            )
        except Exception as exc:
            raise RuntimeError(f"Failed to load MangaOCR ONNX metadata from {model_dir}: {exc}") from exc

        encoder_providers = self._encoder_session.get_providers()
        decoder_providers = self._decoder_session.get_providers()
        self.selected_encoder_provider = encoder_providers[0] if encoder_providers else "unknown"
        self.selected_decoder_provider = decoder_providers[0] if decoder_providers else "unknown"

    @staticmethod
    def _resolve_tokenizer_dir(model_dir: Path) -> Path:
        if any((model_dir / file_name).is_file() for file_name in ("tokenizer.json", "vocab.txt")):
            return model_dir
        fallback_dir = model_dir.parent / MANGA_OCR_MODEL_DIRS["2025_onnx"]
        if any((fallback_dir / file_name).is_file() for file_name in ("tokenizer.json", "vocab.txt")):
            return fallback_dir
        return model_dir

    @staticmethod
    def _build_session(model_path: Path, settings: ProviderSettings):
        if not model_path.is_file():
            raise RuntimeError(f"MangaOCR ONNX file is missing: {model_path}")

        _configure_onnx_cache_environment(resolve_compiled_cache_root(), settings)
        session_options = ort.SessionOptions()
        session_options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        attempts = provider_attempts(settings)
        errors: list[str] = []

        for providers in attempts:
            try:
                return ort.InferenceSession(
                    str(model_path),
                    sess_options=session_options,
                    providers=providers,
                )
            except Exception as exc:
                errors.append(f"{providers}: {exc}")

        details = "\n".join(errors) if errors else "No providers attempted."
        raise RuntimeError(
            "Failed to initialize MangaOCR ONNX Runtime session.\n"
            f"Model: {model_path}\n"
            f"Requested provider: {settings.provider}\n"
            f"Attempts:\n{details}"
        )

    def recognize(self, image) -> str:
        prepared = MangaOcrService._prepare_ocr_image(image)
        encoder_hidden_states = self._run_encoder(prepared)
        token_ids = self._generate_token_ids(encoder_hidden_states)
        decoded = self.tokenizer.decode(list(token_ids), skip_special_tokens=True)
        return MangaOcrService._post_process(decoded)

    def close(self) -> None:
        encoder_session = self._encoder_session
        decoder_session = self._decoder_session
        self._encoder_session = None
        self._decoder_session = None
        if encoder_session is not None:
            del encoder_session
        if decoder_session is not None:
            del decoder_session

    def _run_encoder(self, image) -> np.ndarray:
        pixel_values = self.processor(image, return_tensors="np")["pixel_values"]
        input_array = np.asarray(pixel_values, dtype=np.float32)
        with self._encoder_lock:
            outputs = self._encoder_session.run(
                [self._encoder_output_name],
                {self._encoder_input_name: input_array},
            )
        return np.asarray(outputs[0], dtype=np.float32)

    def _run_decoder(self, input_ids: np.ndarray, encoder_hidden_states: np.ndarray) -> np.ndarray:
        with self._decoder_lock:
            outputs = self._decoder_session.run(
                [self._decoder_output_name],
                {
                    self._decoder_input_ids_name: input_ids,
                    self._decoder_encoder_states_name: encoder_hidden_states,
                },
            )
        return np.asarray(outputs[0], dtype=np.float32)

    def _generate_token_ids(self, encoder_hidden_states: np.ndarray) -> tuple[int, ...]:
        config = self.generation_config
        decoder_start_token_id = self._require_token_id(
            config.decoder_start_token_id,
            "decoder_start_token_id",
        )
        eos_token_id = self._require_token_id(config.eos_token_id, "eos_token_id")
        max_length = max(int(config.max_length or 300), 2)
        num_beams = max(int(config.num_beams or 1), 1)
        no_repeat_ngram_size = max(int(config.no_repeat_ngram_size or 0), 0)
        length_penalty = float(config.length_penalty or 1.0)
        early_stopping = bool(config.early_stopping)

        beams = [_BeamCandidate(token_ids=(decoder_start_token_id,), sum_logprob=0.0, finished=False)]
        completed: list[_BeamCandidate] = []

        for _ in range(max_length - 1):
            candidates: list[_BeamCandidate] = []

            for beam in beams:
                decoder_input_ids = np.asarray([beam.token_ids], dtype=np.int64)
                logits = self._run_decoder(decoder_input_ids, encoder_hidden_states)
                next_token_logits = np.asarray(logits[0, -1], dtype=np.float32)
                next_token_logprobs = _log_softmax(next_token_logits)
                banned = _no_repeat_ngram_banned_tokens(beam.token_ids, no_repeat_ngram_size)
                if banned:
                    next_token_logprobs[list(banned)] = -np.inf

                top_indices = _top_k_indices(next_token_logprobs, num_beams * 2)
                for index in top_indices:
                    token_id = int(index)
                    token_logprob = float(next_token_logprobs[token_id])
                    if not math.isfinite(token_logprob):
                        continue
                    next_ids = beam.token_ids + (token_id,)
                    next_candidate = _BeamCandidate(
                        token_ids=next_ids,
                        sum_logprob=beam.sum_logprob + token_logprob,
                        finished=token_id == eos_token_id,
                    )
                    if next_candidate.finished:
                        completed.append(next_candidate)
                    else:
                        candidates.append(next_candidate)

            if not candidates:
                break

            beams = sorted(
                candidates,
                key=lambda item: item.sum_logprob,
                reverse=True,
            )[:num_beams]

            if early_stopping and len(completed) >= num_beams:
                break

        best_pool = completed or beams
        if not best_pool:
            return (decoder_start_token_id,)
        best = max(best_pool, key=lambda item: item.normalized_score(length_penalty))
        return best.token_ids

    @staticmethod
    def _require_token_id(value: Any, field_name: str) -> int:
        if value is None:
            raise RuntimeError(f"MangaOCR generation config is missing '{field_name}'.")
        return int(value)


class _TorchMangaOcrRuntime:
    def __init__(self, force_cpu: bool) -> None:
        try:
            import torch  # type: ignore
            from manga_ocr.ocr import MangaOcrModel, post_process  # type: ignore
            from transformers import AutoTokenizer, ViTImageProcessor  # type: ignore
        except Exception as exc:
            raise RuntimeError(f"manga_ocr runtime imports failed: {exc}") from exc

        try:
            self._processor = ViTImageProcessor.from_pretrained(
                "kha-white/manga-ocr-base",
                local_files_only=True,
            )
            self._tokenizer = AutoTokenizer.from_pretrained(
                "kha-white/manga-ocr-base",
                local_files_only=True,
            )
            self._model = MangaOcrModel.from_pretrained(
                "kha-white/manga-ocr-base",
                local_files_only=True,
            )
            self._post_process = post_process
        except Exception as exc:
            raise RuntimeError(
                "MangaOCR PyTorch init failed. "
                "The package is available, but its weights are not cached locally or could not be loaded offline: "
                f"{exc}"
            ) from exc

        if not force_cpu and torch.cuda.is_available():
            self._model.cuda()
        elif not force_cpu and torch.backends.mps.is_available():
            self._model.to("mps")
        self._torch = torch

    def recognize(self, image) -> str:
        try:
            prepared = MangaOcrService._prepare_ocr_image(image)
            pixel_values = self._processor(prepared, return_tensors="pt").pixel_values
            with self._torch.no_grad():
                tokens = self._model.generate(
                    pixel_values.to(self._model.device),
                    max_length=300,
                )[0].cpu()
            decoded = self._tokenizer.decode(tokens, skip_special_tokens=True)
            return str(self._post_process(decoded) or "")
        except Exception as exc:
            raise RuntimeError(f"MangaOCR PyTorch inference failed: {exc}") from exc

    def close(self) -> None:
        self._processor = None
        self._tokenizer = None
        self._model = None
        self._post_process = None
        self._torch = None


class MangaOcrService:
    MODEL_KEY_PREFIX = "mangaocr"

    def __init__(
        self,
        model_manager: LoadedModelManager,
        ai_device_service: AiDeviceService,
    ) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._ai_device_service = ai_device_service
        self._runtime: _OnnxMangaOcrRuntime | _TorchMangaOcrRuntime | None = None
        self._runtime_key: str | None = None
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        with self._lock:
            return {
                "ready": self._runtime is not None,
                "backend": self._runtime_backend_name(self._runtime_key),
                "runtime_key": self._runtime_key,
                "last_error": self._last_error,
            }

    def warmup(self) -> None:
        lease_ctx = self._begin_model_use()
        try:
            with self._lock:
                runtime = self._ensure_loaded_locked(lease_ctx.model_key, lease_ctx.settings)
            if lease_ctx.needs_load:
                lease_ctx.mark_loaded(
                    unload_callback=lambda: self._unload_key(lease_ctx.model_key)
                )
            from PIL import Image

            dummy = Image.new("RGB", (8, 8), (0, 0, 0))
            runtime.recognize(dummy)
        except Exception:
            if lease_ctx.needs_load:
                lease_ctx.mark_load_failed()
            raise
        finally:
            lease_ctx.release()

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        manga_model: Any = None,
    ) -> dict[str, Any]:
        lease_ctx = self._begin_model_use(manga_model)
        try:
            with self._lock:
                runtime = self._ensure_loaded_locked(lease_ctx.model_key, lease_ctx.settings)
            if lease_ctx.needs_load:
                lease_ctx.mark_loaded(
                    unload_callback=lambda: self._unload_key(lease_ctx.model_key)
                )

            from PIL import Image

            with Image.open(io.BytesIO(image_bytes)) as img:
                text_raw = runtime.recognize(img)
        except Exception:
            if lease_ctx.needs_load:
                lease_ctx.mark_load_failed()
            raise
        finally:
            lease_ctx.release()

        text = str(text_raw or "")
        lines = [line.strip() for line in text.splitlines() if line.strip()]
        if not lines and text.strip():
            lines = [text.strip()]
        if reflect_strings:
            lines.reverse()

        output_text = "\n".join(lines) if join_newlines else " ".join(lines)
        return {
            "lines": lines,
            "text": output_text.strip(),
        }

    def _begin_model_use(self, manga_model: Any = None) -> _LeaseContext:
        settings = self._selected_provider_settings()
        selected_model = self._normalize_model_name(manga_model)
        model_key = f"{self.MODEL_KEY_PREFIX}:{selected_model}:{settings.cache_key()}"
        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_key(model_key),
        )
        return _LeaseContext(
            lease=lease,
            model_key=model_key,
            settings=settings,
            needs_load=bool(lease.needs_load),
        )

    @staticmethod
    def _models_root() -> Path:
        return Path(__file__).resolve().parents[2] / MODELS_DIR_NAME / ONNX_DIR_NAME / "MangaOCR"

    @classmethod
    def _model_dir(cls, model_name: str) -> Path:
        dir_name = MANGA_OCR_MODEL_DIRS.get(model_name, MANGA_OCR_MODEL_DIRS["base_onnx"])
        return cls._models_root() / dir_name

    @staticmethod
    def _prepare_ocr_image(img):
        from PIL import Image

        rgb = img.convert("L").convert("RGB")
        width, height = rgb.size
        if width >= 2 and height >= 2:
            return rgb

        target_size = (max(2, width), max(2, height))
        resampling = getattr(getattr(Image, "Resampling", Image), "NEAREST")
        return rgb.resize(target_size, resample=resampling)

    @staticmethod
    def _post_process(text: str) -> str:
        without_spaces = "".join(text.split())
        without_ellipsis = without_spaces.replace("…", "...")
        normalized_dots = re.sub(
            r"[・.]{2,}",
            lambda match: "." * (match.end() - match.start()),
            without_ellipsis,
        )
        return jaconv.h2z(normalized_dots, ascii=True, digit=True)

    def _selected_provider_settings(self) -> ProviderSettings:
        state = self._ai_device_service.get_state()
        provider = str(state.get("selected_onnx_provider") or "CPUExecutionProvider").strip()
        device_id = str(state.get("selected_onnx_device_id") or "0").strip() or "0"
        return ProviderSettings(provider=provider, device_id=device_id)

    @staticmethod
    def _normalize_model_name(raw_model: Any) -> str:
        normalized = str(raw_model or "").strip().lower()
        if normalized in {"base_torch", "pytorch", "torch", "base_pytorch", "pytorch_base"}:
            return "base_torch"
        if normalized in {"2025", "2025_onnx", "mangaocr_2025", "manga_ocr_2025"}:
            return "2025_onnx"
        if normalized in {"base", "base_onnx", "onnx", "basic", "default"}:
            return "base_onnx"
        return "base_onnx"

    @staticmethod
    def _runtime_backend_name(model_key: str | None) -> str:
        if not model_key:
            return "none"
        model_name = model_key.split(":")[1] if ":" in model_key else ""
        if model_name == "base_torch":
            return "pytorch"
        return "onnx"

    @staticmethod
    def _torch_force_cpu(selected_device: str) -> bool:
        normalized = selected_device.strip().lower()
        return normalized == "cpu"

    def _ensure_loaded_locked(
        self,
        model_key: str,
        settings: ProviderSettings,
    ) -> _OnnxMangaOcrRuntime | _TorchMangaOcrRuntime:
        if self._runtime is not None and self._runtime_key == model_key:
            return self._runtime

        if self._runtime is not None and self._runtime_key is not None:
            self._unload_locked(self._runtime_key)

        model_name = self._normalize_model_name(model_key.split(":")[1] if ":" in model_key else None)
        try:
            if model_name == "base_torch":
                selected_device = str(
                    self._ai_device_service.get_state().get("selected_device") or "cpu"
                )
                runtime = _TorchMangaOcrRuntime(
                    force_cpu=self._torch_force_cpu(selected_device)
                )
                log.info(
                    "MangaOCR PyTorch runtime ready: model=%s selected_device=%s",
                    model_name,
                    selected_device,
                )
            else:
                model_dir = self._model_dir(model_name)
                if not model_dir.is_dir():
                    self._last_error = f"MangaOCR ONNX directory is missing: {model_dir}"
                    raise RuntimeError(self._last_error)
                runtime = _OnnxMangaOcrRuntime(model_dir, settings)
                log.info(
                    "MangaOCR ONNX runtime ready: model=%s model_dir=%s provider=%s device=%s encoder_provider=%s decoder_provider=%s",
                    model_name,
                    model_dir,
                    settings.provider,
                    settings.device_id,
                    runtime.selected_encoder_provider,
                    runtime.selected_decoder_provider,
                )
        except Exception as exc:
            self._runtime = None
            self._runtime_key = None
            self._last_error = f"MangaOCR init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._runtime = runtime
        self._runtime_key = model_key
        self._last_error = None
        return runtime

    def _unload_key(self, model_key: str) -> bool:
        with self._lock:
            return self._unload_locked(model_key)

    def _unload_locked(self, model_key: str) -> bool:
        if self._runtime is None or self._runtime_key != model_key:
            return False

        runtime = self._runtime
        self._runtime = None
        self._runtime_key = None
        runtime.close()
        _clear_runtime_cache()
        self._model_manager.mark_unloaded(model_key)
        log.info("MangaOCR runtime unloaded: model_key=%s", model_key)
        return True


def _log_softmax(logits: np.ndarray) -> np.ndarray:
    logits_float = np.asarray(logits, dtype=np.float32)
    max_logit = float(np.max(logits_float))
    stabilized = logits_float - max_logit
    exp_values = np.exp(stabilized)
    exp_sum = float(np.sum(exp_values))
    if exp_sum <= 0.0 or not math.isfinite(exp_sum):
        return np.full_like(logits_float, -np.inf)
    return stabilized - math.log(exp_sum)


def _top_k_indices(values: np.ndarray, limit: int) -> list[int]:
    if limit <= 0:
        return []
    normalized = np.asarray(values, dtype=np.float32)
    k = min(limit, int(normalized.shape[0]))
    if k <= 0:
        return []
    partition = np.argpartition(normalized, -k)[-k:]
    ordered = partition[np.argsort(normalized[partition])[::-1]]
    return [int(index) for index in ordered]


def _no_repeat_ngram_banned_tokens(token_ids: tuple[int, ...], ngram_size: int) -> set[int]:
    if ngram_size <= 0 or len(token_ids) < ngram_size - 1:
        return set()

    prefix = token_ids[-(ngram_size - 1) :] if ngram_size > 1 else tuple()
    banned: set[int] = set()
    max_start = len(token_ids) - ngram_size + 1
    for start in range(max_start):
        ngram = token_ids[start : start + ngram_size]
        if ngram_size == 1 or ngram[:-1] == prefix:
            banned.add(int(ngram[-1]))
    return banned
