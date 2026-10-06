"""
FILE OVERVIEW: modules/ai_backend/ocr/baberu.py
Baberu OCR (`ocr.baberu`): speech-bubble OCR for Japanese, Chinese and English
run from its ONNX export: vision on the selected ONNX provider, decoders on CPU.

Main responsibilities:
- validate the four absolute model-file paths sent by Rust (`BaberuFiles`);
- build and reuse the vision / decoder-prefill / decoder-step sessions, placed by
  `vision_provider_settings` (vision) and on `CPUExecutionProvider` (decoders);
- preprocess a crop exactly like upstream `onnx_infer.py` (`preprocess`);
- run the upstream greedy decode with its repetition penalty and run caps
  (`greedy_decode`) and map ids back to characters (`BaberuVocab`);
- lease the resident sessions from the shared `LoadedModelManager`.

Key structures:
- `BaberuOcrService`, `BaberuFiles`, `BaberuVocab`

Key functions:
- `preprocess()`, `greedy_decode()`, `vision_provider_settings()`

Notes:
- This is the backend FALLBACK of the native Rust engine
  (`ms_onnx::BaberuOcrEngine`). Both are ports of upstream
  `genshiai-daichi/baberu-ocr` `onnx_infer.py` and must keep the decode
  constants identical: repetition penalty 1.2 over every seen id including BOS,
  run caps 12 (one letter/digit) / 16 (any other non-special id), EOS 2, at most
  256 new tokens, step position ids starting at `vision_len + 1` (= 257).
- Vision on the selected EP, decoders on CPU, in both runtimes (why: int8 decoder
  ops fall back to CPU on GPU EPs; benchmark 2026-10-06). TensorRT is replaced by
  the CUDA provider for vision (no engine cache; each session build would compile
  for minutes). A vision session that fails on its provider, or that ONNX Runtime
  silently put on the CPU (missing GPU libraries; checked with `get_providers()`),
  runs on the CPU with a WARN: CPU is a correct vision backend.
- The lease key is `baberuocr:<provider>:<device>` of the selection; a selection
  change rebuilds the sessions under the new key and reports the old key unloaded.
- The backend never downloads Baberu and holds no layout: Rust owns the pinned
  download and sends absolute file paths per request (`baberu_model_files`).
- `onnxruntime`, Pillow and `engines.paddle_onnx` (provider specs; it imports
  cv2) are imported lazily so importing this module stays cheap for the
  composition root and the torch-free `ipc/` tests.
"""

from __future__ import annotations

import io
import json
import logging
import threading
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Mapping

import numpy as np

from ..runtime.model_manager import LoadedModelManager
from .result_format import format_recognition_lines

if TYPE_CHECKING:
    from ..engines.paddle_onnx import ProviderSettings
    from ..runtime.device_service import AiDeviceService

log = logging.getLogger(__name__)

# DINOv2 / ImageNet preprocessing: the whole crop is resized to 224x224 (no
# aspect preservation, no center crop), as in upstream `onnx_infer.py`.
INPUT_SIZE = 224
IMAGENET_MEAN = np.array([0.485, 0.456, 0.406], np.float32)
IMAGENET_STD = np.array([0.229, 0.224, 0.225], np.float32)

# Token ids 0..3 are <pad>/<bos>/<eos>/<unk>; id >= 4 is charset[id - 4].
SPECIAL_ID_COUNT = 4
BOS_ID = 1
EOS_ID = 2

# Published decode settings (upstream `BaberuOnnxOCR.__call__` defaults).
MAX_NEW_TOKENS = 256
REPETITION_PENALTY = 1.2
MAX_CONTENT_RUN = 12
MAX_SYMBOL_RUN = 16
# Long-vowel / wave marks repeat legitimately, so they are capped as symbols.
NON_CONTENT_CHARS = frozenset("ーｰ〜~")

# The decoder exports carry 6 layers of K/V cache, named `present_k{i}` /
# `present_v{i}` on output and `past_k{i}` / `past_v{i}` on the step input.
KV_LAYERS = 6
PRESENT_TO_PAST: dict[str, str] = {
    f"present_{kind}{layer}": f"past_{kind}{layer}"
    for kind in ("k", "v")
    for layer in range(KV_LAYERS)
}

# Keys of the `baberu_model_files` request object, in `BaberuFiles` field order.
MODEL_FILE_FIELDS = ("vision", "prefill", "step", "vocab")

CPU_PROVIDER = "CPUExecutionProvider"
TENSORRT_PROVIDER = "TensorrtExecutionProvider"
CUDA_PROVIDER = "CUDAExecutionProvider"
# Providers whose session spec carries `device_id` (mirrors `paddle_onnx.provider_spec`);
# the health `device` names the adapter only for these.
DEVICE_INDEXED_PROVIDERS = frozenset({CUDA_PROVIDER, "DmlExecutionProvider", "MIGraphXExecutionProvider"})


@dataclass(frozen=True)
class BaberuFiles:
    """Absolute paths of the four Baberu model files, as sent by Rust.

    Equality of two instances is what decides whether resident sessions can be
    reused or must be rebuilt.
    """

    vision: Path
    prefill: Path
    step: Path
    vocab: Path

    @classmethod
    def from_request(cls, raw: Mapping[str, Any]) -> "BaberuFiles":
        """Validate the request's `{vision, prefill, step, vocab}` path object.

        Raises ValueError naming the entry when it is absent, not a non-empty
        string, not absolute, or not an existing file.
        """
        resolved: dict[str, Path] = {}
        for name in MODEL_FILE_FIELDS:
            value = raw.get(name)
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"Baberu model file '{name}' must be a non-empty path string.")
            path = Path(value)
            if not path.is_absolute():
                raise ValueError(f"Baberu model file '{name}' must be an absolute path: {path}")
            if not path.is_file():
                raise ValueError(f"Baberu model file '{name}' does not exist: {path}")
            resolved[name] = path
        return cls(**resolved)


class BaberuVocab:
    """Character-level Baberu vocabulary (`tokenizer/vocab.json`).

    The JSON is a list of single characters; id `i + 4` decodes to entry `i`.
    A "content" id (capped at `MAX_CONTENT_RUN`) is a letter or digit (Unicode
    category L* or N*) other than `NON_CONTENT_CHARS`.
    """

    def __init__(self, charset: list[str]) -> None:
        """Raise ValueError when an entry is not exactly one character."""
        for index, entry in enumerate(charset):
            if not isinstance(entry, str) or len(entry) != 1:
                raise ValueError(f"Baberu vocab entry {index} is not a single character: {entry!r}")
        self._chars = list(charset)
        self._content = [
            ch not in NON_CONTENT_CHARS and unicodedata.category(ch)[0] in "LN"
            for ch in self._chars
        ]

    @classmethod
    def from_json_bytes(cls, data: bytes) -> "BaberuVocab":
        """Parse `vocab.json` bytes; ValueError on bad JSON or a non-list document."""
        try:
            charset = json.loads(data.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ValueError(f"Baberu vocab is not valid UTF-8 JSON: {exc}") from exc
        if not isinstance(charset, list):
            raise ValueError("Baberu vocab must be a JSON list of characters.")
        return cls(charset)

    @classmethod
    def from_file(cls, path: Path) -> "BaberuVocab":
        """Read and parse `path`; OSError / ValueError propagate with the path."""
        try:
            data = path.read_bytes()
        except OSError as exc:
            raise OSError(f"Cannot read Baberu vocab {path}: {exc}") from exc
        try:
            return cls.from_json_bytes(data)
        except ValueError as exc:
            raise ValueError(f"{exc} (file: {path})") from exc

    @property
    def vocab_size(self) -> int:
        """Total id count, special ids included."""
        return len(self._chars) + SPECIAL_ID_COUNT

    def is_content(self, token_id: int) -> bool:
        """True for a letter/digit id; False for specials, symbols and unknown ids."""
        index = token_id - SPECIAL_ID_COUNT
        return 0 <= index < len(self._content) and self._content[index]

    def decode(self, token_ids: list[int]) -> str:
        """Concatenate the characters of `token_ids`, skipping specials and unknown ids."""
        size = len(self._chars)
        return "".join(
            self._chars[token_id - SPECIAL_ID_COUNT]
            for token_id in token_ids
            if 0 <= token_id - SPECIAL_ID_COUNT < size
        )


def preprocess(image: Any) -> np.ndarray:
    """Turn a PIL image into the `[1, 3, 224, 224]` float32 `pixel_values` tensor.

    Alpha is dropped (`convert("RGB")`), the whole crop is Pillow-BICUBIC resized
    to 224x224, scaled to [0, 1] and ImageNet-normalized, then laid out CHW.
    """
    from PIL import Image

    resample = getattr(getattr(Image, "Resampling", Image), "BICUBIC")
    rgb = image.convert("RGB").resize((INPUT_SIZE, INPUT_SIZE), resample)
    pixels = (np.asarray(rgb, np.float32) / 255.0 - IMAGENET_MEAN) / IMAGENET_STD
    return np.ascontiguousarray(pixels.transpose(2, 0, 1)[None], dtype=np.float32)


def greedy_decode(
    prefill_logits: np.ndarray,
    step_fn: Callable[[int, int], np.ndarray],
    is_content: Callable[[int], bool],
    *,
    first_position: int,
    max_new_tokens: int = MAX_NEW_TOKENS,
    repetition_penalty: float = REPETITION_PENALTY,
    max_content_run: int = MAX_CONTENT_RUN,
    max_symbol_run: int = MAX_SYMBOL_RUN,
) -> list[int]:
    """Greedy Baberu decode; returns the generated ids without BOS and EOS.

    `prefill_logits` is the last-position logits row of the prefill pass.
    `step_fn(token_id, position)` feeds one token at `position` (the first call
    gets `first_position`, then +1 per step) and returns that step's logits row.
    Per step: every seen id (BOS included) is penalized HF-style (divided when
    >= 0, multiplied when < 0); if the trailing run of the last token reached
    its cap (`max_content_run` when `is_content`, `max_symbol_run` for any other
    id > 3) that token is banned; argmax takes the FIRST maximum. Stops on EOS
    or after `max_new_tokens` tokens. Raises RuntimeError on NaN logits.
    """
    logits = np.array(prefill_logits, dtype=np.float64)
    seen = {BOS_ID}
    tokens: list[int] = []
    position = first_position
    for _ in range(max_new_tokens):
        if repetition_penalty != 1.0:
            for token_id in seen:
                score = logits[token_id]
                logits[token_id] = score * repetition_penalty if score < 0 else score / repetition_penalty
        last = tokens[-1] if tokens else 0
        if is_content(last):
            cap = max_content_run
        elif last >= SPECIAL_ID_COUNT:
            cap = max_symbol_run
        else:
            cap = 0
        if cap:
            run = 0
            for token_id in reversed(tokens):
                if token_id != last:
                    break
                run += 1
            if run >= cap:
                logits[last] = -np.inf
        if np.isnan(logits).any():
            raise RuntimeError(f"Baberu decoder produced NaN logits at generated token {len(tokens)}.")
        next_id = int(np.argmax(logits))
        if next_id == EOS_ID:
            break
        tokens.append(next_id)
        seen.add(next_id)
        if len(tokens) >= max_new_tokens:
            break
        logits = np.array(step_fn(next_id, position), dtype=np.float64)
        position += 1
    return tokens


def vision_provider_settings(selected: "ProviderSettings") -> "ProviderSettings":
    """The provider settings the vision graph is built with for the `selected` ones.

    The selection itself, except TensorRT -> CUDA on the same device: without a
    persistent engine cache every TensorRT session build compiles for minutes
    (benchmark 2026-10-06: 374 s for the vision graph). Decoders never use this:
    they are always on `CPUExecutionProvider`.
    """
    from ..engines.paddle_onnx import ProviderSettings

    if selected.provider == TENSORRT_PROVIDER:
        return ProviderSettings(provider=CUDA_PROVIDER, device_id=selected.device_id)
    return selected


def _device_label(provider: str, device_id: str) -> str:
    """Health `device` text: `Provider:<id>` for device-indexed providers, else the name."""
    return f"{provider}:{device_id}" if provider in DEVICE_INDEXED_PROVIDERS else provider


def _session_options(ort: Any) -> Any:
    """Session options of every Baberu graph (the reference's `ORT_ENABLE_ALL`)."""
    options = ort.SessionOptions()
    options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    return options


def _build_cpu_session(ort: Any, model_path: Path) -> Any:
    """Build an ORT session on the CPU execution provider only."""
    try:
        return ort.InferenceSession(
            str(model_path),
            sess_options=_session_options(ort),
            providers=[CPU_PROVIDER],
        )
    except Exception as exc:
        raise RuntimeError(f"Failed to create the Baberu ONNX session for {model_path}: {exc}") from exc


def _build_vision_session(ort: Any, model_path: Path, selected: "ProviderSettings") -> tuple[Any, str]:
    """Build the vision session per `vision_provider_settings`; return it and its device label.

    Falls back to the CPU session with a WARN naming requested vs effective
    provider when the provider is not available in this onnxruntime, when the
    session cannot be built on it, or when ONNX Runtime silently dropped it
    (`get_providers()[0]` differs). RuntimeError only when the CPU build fails.
    """
    from ..engines.paddle_onnx import (
        _configure_onnx_cache_environment,
        provider_attempts,
        resolve_compiled_cache_root,
    )

    used = vision_provider_settings(selected)
    if used.provider != selected.provider:
        log.info(
            "Baberu OCR vision: TensorRT is not used for Baberu (no engine cache; each session build "
            "would compile for minutes); vision on %s device=%s instead.",
            used.provider,
            used.device_id,
        )
    if used.provider == CPU_PROVIDER:
        return _build_cpu_session(ort, model_path), CPU_PROVIDER

    failure: str | None = None
    attempts = provider_attempts(used)
    if not attempts:
        failure = "the provider is not available in this onnxruntime"
    else:
        if used.is_migraphx():
            # MIGraphX compiles the graph; give it the shared per-provider cache dirs
            # exactly like the other ONNX services.
            _configure_onnx_cache_environment(resolve_compiled_cache_root(), used)
        try:
            session = ort.InferenceSession(
                str(model_path),
                sess_options=_session_options(ort),
                providers=attempts[0],
            )
        except Exception as exc:
            failure = f"session build failed: {exc}"
        else:
            active = session.get_providers()
            effective = active[0] if active else "unknown"
            if effective == used.provider:
                return session, _device_label(used.provider, used.device_id)
            failure = f"onnxruntime silently ran it on {effective} (GPU libraries missing?)"
            if effective == CPU_PROVIDER:
                log.warning(
                    "Baberu OCR vision requested provider=%s device=%s but effective provider=%s: %s. "
                    "Recognition stays correct, vision is slower.",
                    selected.provider,
                    selected.device_id,
                    effective,
                    failure,
                )
                return session, CPU_PROVIDER

    log.warning(
        "Baberu OCR vision requested provider=%s device=%s (tried %s) but effective provider=%s: %s. "
        "Recognition stays correct, vision is slower.",
        selected.provider,
        selected.device_id,
        used.provider,
        CPU_PROVIDER,
        failure,
    )
    return _build_cpu_session(ort, model_path), CPU_PROVIDER


def _require_io(session: Any, model_path: Path, inputs: set[str], outputs: set[str]) -> None:
    """Raise RuntimeError naming the file when the graph lacks an expected input/output."""
    have_inputs = {node.name for node in session.get_inputs()}
    have_outputs = {node.name for node in session.get_outputs()}
    missing = sorted(inputs - have_inputs) + sorted(outputs - have_outputs)
    if missing:
        raise RuntimeError(
            f"Baberu ONNX graph {model_path} lacks the expected inputs/outputs {missing}; "
            "the file does not match the pinned Baberu export."
        )


def _run_named(session: Any, feed: dict[str, np.ndarray]) -> dict[str, np.ndarray]:
    """Run `session` and return every output keyed by its graph name."""
    names = [node.name for node in session.get_outputs()]
    return dict(zip(names, session.run(names, feed)))


def _past_from(outputs: dict[str, np.ndarray]) -> dict[str, np.ndarray]:
    """Map a pass's `present_*` cache outputs onto the next step's `past_*` inputs by name."""
    return {past: outputs[present] for present, past in PRESENT_TO_PAST.items()}


class _BaberuOnnxRuntime:
    """The vision session (selected provider) and two CPU decoder sessions plus
    vocab for one `BaberuFiles` set and provider selection.

    `vision_device` is the effective vision device label. `recognize` keeps its
    K/V cache in locals, so concurrent calls on one instance are safe (ORT
    sessions are thread-safe for `run`).
    """

    def __init__(self, files: BaberuFiles, settings: "ProviderSettings") -> None:
        """Load every file; RuntimeError / ValueError / OSError name the failing file."""
        try:
            import onnxruntime as ort  # type: ignore
        except Exception as exc:
            raise RuntimeError(f"onnxruntime is not available: {exc}") from exc

        self.files = files
        self._vocab = BaberuVocab.from_file(files.vocab)
        self._vision, self.vision_device = _build_vision_session(ort, files.vision, settings)
        self._prefill = _build_cpu_session(ort, files.prefill)
        self._step = _build_cpu_session(ort, files.step)
        presents = set(PRESENT_TO_PAST)
        _require_io(self._vision, files.vision, {"pixel_values"}, {"vision_embeds"})
        _require_io(self._prefill, files.prefill, {"vision_embeds", "input_ids"}, {"logits"} | presents)
        _require_io(
            self._step,
            files.step,
            {"input_ids", "position_ids"} | set(PRESENT_TO_PAST.values()),
            {"logits"} | presents,
        )

    def recognize(self, image: Any) -> str:
        """Recognize one bubble crop; newlines produced by the model are kept."""
        pixel_values = preprocess(image)
        vision_embeds = self._vision.run(["vision_embeds"], {"pixel_values": pixel_values})[0]
        prefill = _run_named(
            self._prefill,
            {"vision_embeds": vision_embeds, "input_ids": np.array([[BOS_ID]], np.int64)},
        )
        past = _past_from(prefill)

        def step(token_id: int, position: int) -> np.ndarray:
            nonlocal past
            feed = {
                "input_ids": np.array([[token_id]], np.int64),
                "position_ids": np.array([[position]], np.int64),
            }
            feed.update(past)
            outputs = _run_named(self._step, feed)
            past = _past_from(outputs)
            return outputs["logits"][0, -1]

        # The prefill consumed `vision_len` image tokens plus BOS, so the first
        # generated token sits at position `vision_len + 1`.
        token_ids = greedy_decode(
            prefill["logits"][0, -1],
            step,
            self._vocab.is_content,
            first_position=int(vision_embeds.shape[1]) + 1,
        )
        return self._vocab.decode(token_ids)


def _decode_image(image_bytes: bytes) -> Any:
    """Decode the request image to RGB; ValueError when Pillow cannot read it."""
    from PIL import Image, UnidentifiedImageError

    try:
        with Image.open(io.BytesIO(image_bytes)) as img:
            return img.convert("RGB")
    except (UnidentifiedImageError, OSError) as exc:
        raise ValueError(f"Baberu OCR could not decode the request image: {exc}") from exc


class BaberuOcrService:
    """Baberu OCR leased under `baberuocr:<provider>:<device>` of the ONNX selection.

    One (`BaberuFiles`, selection) runtime is resident at a time: a request naming
    different files rebuilds the sessions under the same key; a changed provider
    selection rebuilds them under the new key and reports the old key unloaded.
    """

    MODEL_KEY_PREFIX = "baberuocr"

    def __init__(self, model_manager: LoadedModelManager, ai_device_service: "AiDeviceService") -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._ai_device_service = ai_device_service
        self._runtime: _BaberuOnnxRuntime | None = None
        self._runtime_key: str | None = None
        self._last_error: str | None = None

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "device", "last_error"}` for the health snapshot.

        `device` is the effective vision device of the resident runtime (e.g.
        `CUDAExecutionProvider:0`, `CPUExecutionProvider`), None when none is
        resident; the decoders always run on the CPU.
        """
        with self._lock:
            return {
                "ready": self._runtime is not None,
                "device": self._runtime.vision_device if self._runtime is not None else None,
                "last_error": self._last_error,
            }

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        model_files: Mapping[str, Any],
        join_newlines: bool = True,
        reflect_strings: bool = False,
    ) -> dict[str, Any]:
        """Recognize one bubble crop and return `{"lines", "text"}`.

        `model_files` is the request's `{vision, prefill, step, vocab}` object of
        absolute paths. Raises ValueError for invalid paths or an undecodable
        image, RuntimeError when the sessions cannot load or inference fails.
        """
        files = BaberuFiles.from_request(model_files)
        image = _decode_image(image_bytes)
        settings = self._selected_provider_settings()
        model_key = f"{self.MODEL_KEY_PREFIX}:{settings.cache_key()}"

        # Lease before the instance lock: an eviction callback of another
        # service may hold the manager while waiting for this lock.
        lease = self._model_manager.begin_model_use(model_key, unload_callback=lambda: self._unload_key(model_key))
        try:
            with self._lock:
                # Load scope: only a failure in here is a failed LOAD.
                try:
                    runtime = self._ensure_loaded_locked(
                        files, settings, model_key, lease_needs_load=lease.needs_load
                    )
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
            # Resident from here on, so it is registered before inference: an
            # inference failure must leave it counted and evictable.
            if lease.needs_load:
                lease.mark_loaded(unload_callback=lambda: self._unload_key(model_key))
            try:
                text = runtime.recognize(image)
            except Exception as exc:
                message = f"Baberu OCR inference failed: {exc}"
                log.error("Baberu OCR inference failed: vision=%s error=%s", files.vision, exc)
                with self._lock:
                    self._last_error = message
                raise RuntimeError(message) from exc
        finally:
            lease.release()

        with self._lock:
            self._last_error = None
        return format_recognition_lines(
            text,
            join_newlines=join_newlines,
            reflect_strings=reflect_strings,
        )

    def _selected_provider_settings(self) -> "ProviderSettings":
        """The resolved ONNX provider/device selection (never the `not-selected` sentinel:
        `AiDeviceService.get_state` resolves it to a runtime default without persisting)."""
        from ..engines.paddle_onnx import ProviderSettings

        state = self._ai_device_service.get_state()
        provider = str(state.get("selected_onnx_provider") or CPU_PROVIDER).strip() or CPU_PROVIDER
        device_id = str(state.get("selected_onnx_device_id") or "0").strip() or "0"
        return ProviderSettings(provider=provider, device_id=device_id)

    def _ensure_loaded_locked(
        self,
        files: BaberuFiles,
        settings: "ProviderSettings",
        model_key: str,
        *,
        lease_needs_load: bool,
    ) -> _BaberuOnnxRuntime:
        """Return the runtime for (`files`, `model_key`), rebuilding it when either differs.

        Caller holds `self._lock`. A runtime resident under ANOTHER key (the
        selection changed) is dropped and that key reported unloaded, so the
        manager never counts a phantom resident. A rebuild under a lease that
        found the key already resident (`lease_needs_load=False`) that then
        FAILS reports the key unloaded itself, because the caller's lease has no
        load to abort.
        """
        if self._runtime is not None and self._runtime_key == model_key and self._runtime.files == files:
            return self._runtime

        # Drop the previous sessions first so two session sets never coexist.
        previous_key = self._runtime_key
        self._runtime = None
        self._runtime_key = None
        if previous_key is not None and previous_key != model_key:
            self._model_manager.mark_unloaded(previous_key)
            log.info("Baberu OCR runtime unloaded for a new provider selection: old=%s new=%s", previous_key, model_key)
        try:
            runtime = _BaberuOnnxRuntime(files, settings)
        except Exception as exc:
            self._last_error = f"Baberu OCR init failed: {exc}"
            log.error("Baberu OCR init failed: model_key=%s vision=%s error=%s", model_key, files.vision, exc)
            if not lease_needs_load:
                self._model_manager.mark_unloaded(model_key)
            raise RuntimeError(self._last_error) from exc

        self._runtime = runtime
        self._runtime_key = model_key
        self._last_error = None
        log.info(
            "Baberu OCR runtime ready: model_key=%s vision_device=%s decoders=%s vision=%s prefill=%s step=%s",
            model_key,
            runtime.vision_device,
            CPU_PROVIDER,
            files.vision,
            files.prefill,
            files.step,
        )
        return runtime

    def _unload_key(self, model_key: str) -> bool:
        """Eviction callback for `model_key`: drop the sessions; False when that key is not resident."""
        with self._lock:
            if self._runtime is None or self._runtime_key != model_key:
                return False
            self._runtime = None
            self._runtime_key = None
            self._model_manager.mark_unloaded(model_key)
        log.info("Baberu OCR runtime unloaded: model_key=%s", model_key)
        return True
