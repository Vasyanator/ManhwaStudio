"""
FILE OVERVIEW: modules/ai_backend/paddle_vl_ocr_service.py
OCR service for PaddleOCR-VL run through the Hugging Face Transformers runtime.

Main responsibilities:
- lazy init and health reporting for the PaddleOCR-VL vision-language model;
- single-image text recognition from raw image bytes with a fixed OCR prompt
  (PaddleOCR-VL needs no separate text detection and no language selection);
- synchronization of the model device with backend `General.ai_device`;
- cooperation with `LoadedModelManager` for bounded resident model count.

Notes:
- The model is loaded with `trust_remote_code=True` because transformers 4.57
  has no built-in `paddleocr_vl` architecture; weights are fetched into the
  Hugging Face hub cache on first use (like EasyOCR/Surya), not the app model
  tree.
- This runtime is PyTorch-only; the server gates the endpoint behind a Torch
  availability check.
- The model's remote code was saved with transformers 4.55 (`config.json`
  `transformers_version`). Later transformers releases renamed/restructured two
  internal helpers it imports (`create_causal_mask`'s `inputs_embeds` keyword
  became `input_embeds`; `check_model_inputs` became a decorator factory).
  `_ensure_transformers_compat()` installs signature-guarded aliases for those
  before the remote module is imported, so the engine runs on the app's
  transformers 4.57.x without a global downgrade. The shims are no-ops when the
  installed transformers already matches the remote code; remove them once
  PaddleOCR-VL ships remote code compatible with current transformers.
"""

from __future__ import annotations

import gc
import io
import threading
from typing import Any

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from .model_manager import LoadedModelManager
from .script_constraint import ScriptConstraint, TokenByteIndex, normalize_script

# Hugging Face repository that ships PaddleOCR-VL weights plus the custom model
# code used through `trust_remote_code=True`.
PADDLE_VL_MODEL_ID = "PaddlePaddle/PaddleOCR-VL"
# Fixed prompt PaddleOCR-VL uses for plain text recognition (matches the
# official PaddleOCR-VL pipeline `text_prompt = "OCR:"`).
PADDLE_VL_OCR_PROMPT = "OCR:"
PADDLE_VL_MAX_NEW_TOKENS = 8192
# Safety cap for script-constrained mode: hard restriction can push the model
# into a non-terminating ramble on mismatched input, so bound the output length.
PADDLE_VL_CONSTRAINED_MAX_NEW_TOKENS = 1024


def _clear_torch_cache() -> None:
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


class PaddleVlOcrService:
    """PaddleOCR-VL OCR runtime backed by Hugging Face Transformers.

    The model and processor are loaded lazily per resolved device and shared
    across requests; `LoadedModelManager` may evict them when idle to keep the
    resident model count within the configured limit.
    """

    MODEL_KEY_PREFIX = "paddlevlocr:model"

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._model = None
        self._processor = None
        self._device: str | None = None
        self._last_error: str | None = None
        # Lazily built once per loaded tokenizer; reused across script modes.
        self._token_index: TokenByteIndex | None = None
        self._constraints: dict[str, ScriptConstraint] = {}

    def health(self) -> dict[str, Any]:
        """Return a JSON-friendly readiness snapshot for the health endpoint."""
        with self._lock:
            return {
                "ready": self._model is not None,
                "device": self._device,
                "last_error": self._last_error,
            }

    def warmup(self) -> None:
        """Force-load the model by recognizing a tiny dummy image."""
        from PIL import Image

        dummy = Image.new("RGB", (32, 32), (255, 255, 255))
        encoded = io.BytesIO()
        dummy.save(encoded, format="PNG")
        self.recognize_image_bytes(encoded.getvalue())

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        script: str | None = None,
    ) -> dict[str, Any]:
        """Recognize text in a single image and return `{"lines", "text"}`.

        `join_newlines=False` collapses recognized lines with spaces instead of
        newlines; `reflect_strings=True` reverses line order for right-to-left
        manga column reading. `script` (`korean`/`chinese`/`japanese`, or None for
        auto) hard-restricts generation to that writing system plus whitespace,
        digits, and common punctuation. Raises RuntimeError when the model cannot
        load.
        """
        image = self._decode_image(image_bytes)
        normalized_script = normalize_script(script)
        selected_device = _resolve_selected_backend_device(self._device or "cpu")
        model_key = self._model_key(selected_device)

        lease = self._model_manager.begin_model_use(
            model_key,
            unload_callback=lambda: self._unload_model_key(model_key),
        )
        try:
            with self._lock:
                model, processor = self._ensure_loaded_locked(selected_device)
                constraint = (
                    self._constraint_for_script_locked(processor, normalized_script)
                    if normalized_script is not None
                    else None
                )
            if lease.needs_load:
                lease.mark_loaded(
                    unload_callback=lambda: self._unload_model_key(model_key)
                )
            text = self._generate_text(model, processor, image, constraint)
        except Exception:
            if lease.needs_load:
                lease.mark_load_failed()
            raise
        finally:
            lease.release()

        return _format_recognition_lines(
            text,
            join_newlines=join_newlines,
            reflect_strings=reflect_strings,
        )

    def _constraint_for_script_locked(
        self, processor, script: str
    ) -> ScriptConstraint:
        """Return a cached `ScriptConstraint` for `script`, building the shared
        per-tokenizer byte index on first use. Caller holds `self._lock`."""
        if self._token_index is None:
            self._token_index = TokenByteIndex(processor.tokenizer)
            self._constraints = {}
        constraint = self._constraints.get(script)
        if constraint is None:
            constraint = ScriptConstraint(self._token_index, script)
            self._constraints[script] = constraint
        return constraint

    def _generate_text(self, model, processor, image, constraint=None) -> str:
        """Run a single generate pass and decode only the newly generated tokens.

        When `constraint` is set, generation is hard-restricted to its writing
        system via a stateful UTF-8 `prefix_allowed_tokens_fn`."""
        import torch  # type: ignore

        messages = [
            {
                "role": "user",
                "content": [
                    {"type": "image", "image": image},
                    {"type": "text", "text": PADDLE_VL_OCR_PROMPT},
                ],
            }
        ]
        prompt = processor.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True
        )
        inputs = processor(text=[prompt], images=[image], return_tensors="pt")
        inputs = inputs.to(model.device)

        generate_kwargs: dict[str, Any] = {"max_new_tokens": PADDLE_VL_MAX_NEW_TOKENS}
        if constraint is not None:
            prompt_len = int(inputs["input_ids"].shape[1])
            generate_kwargs["prefix_allowed_tokens_fn"] = constraint.prefix_fn(
                prompt_len
            )
            generate_kwargs["max_new_tokens"] = PADDLE_VL_CONSTRAINED_MAX_NEW_TOKENS

        with torch.no_grad():
            generated_ids = model.generate(**inputs, **generate_kwargs)

        # Drop the prompt tokens so only the model's answer is decoded.
        input_ids = inputs["input_ids"]
        trimmed = [
            output_ids[len(prompt_ids):]
            for prompt_ids, output_ids in zip(input_ids, generated_ids)
        ]
        decoded = processor.batch_decode(
            trimmed,
            skip_special_tokens=True,
            clean_up_tokenization_spaces=False,
        )
        return str(decoded[0] if decoded else "").strip()

    def _ensure_loaded_locked(self, device: str):
        """Load (or reuse) the model and processor for `device`. Caller holds lock."""
        if (
            self._model is not None
            and self._processor is not None
            and self._device == device
        ):
            return self._model, self._processor

        if self._device is not None and self._device != device:
            self._drop_model_locked()

        try:
            import torch  # type: ignore
            from transformers import AutoModelForCausalLM, AutoProcessor
        except Exception as exc:
            self._last_error = f"PaddleOCR-VL dependencies are not available: {exc}"
            raise RuntimeError(self._last_error) from exc

        # Must run before from_pretrained imports the model's remote module.
        _ensure_transformers_compat()
        dtype = self._resolve_dtype(torch, device)
        try:
            processor = AutoProcessor.from_pretrained(
                PADDLE_VL_MODEL_ID, trust_remote_code=True
            )
            # PaddleOCR-VL's config.json `auto_map` registers the generative model
            # under `AutoModel`/`AutoModelForCausalLM` (not ImageTextToText), so the
            # remote `PaddleOCRVLForConditionalGeneration` is resolved through here.
            model = AutoModelForCausalLM.from_pretrained(
                PADDLE_VL_MODEL_ID,
                trust_remote_code=True,
                dtype=dtype,
            )
            model = model.to(device)
            model.eval()
        except Exception as exc:
            self._model = None
            self._processor = None
            self._device = None
            self._last_error = f"PaddleOCR-VL init failed: {exc}"
            raise RuntimeError(self._last_error) from exc

        self._model = model
        self._processor = processor
        self._device = device
        self._last_error = None
        return model, processor

    @staticmethod
    def _resolve_dtype(torch_module, device: str):
        """Pick bfloat16 on capable CUDA, float32 elsewhere for stable CPU output."""
        if device.startswith("cuda"):
            try:
                if torch_module.cuda.is_bf16_supported():
                    return torch_module.bfloat16
            except Exception:
                pass
            return torch_module.float16
        return torch_module.float32

    @staticmethod
    def _decode_image(image_bytes: bytes):
        from PIL import Image

        with Image.open(io.BytesIO(image_bytes)) as img:
            rgb = img.convert("RGB")
            width, height = rgb.size
            if width >= 2 and height >= 2:
                return rgb

            resampling = getattr(getattr(Image, "Resampling", Image), "NEAREST")
            target_size = (max(2, width), max(2, height))
            return rgb.resize(target_size, resample=resampling)

    def _unload_model_key(self, model_key: str) -> bool:
        with self._lock:
            current_device = self._device
            if current_device is None or model_key != self._model_key(current_device):
                return False
            self._drop_model_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
            return True

    def _drop_model_locked(self) -> None:
        self._model = None
        self._processor = None
        self._device = None
        # Tokenizer/byte-index belongs to the dropped processor; rebuild on reload.
        self._token_index = None
        self._constraints = {}

    @classmethod
    def _model_key(cls, device: str) -> str:
        return f"{cls.MODEL_KEY_PREFIX}:{device}"


def _ensure_transformers_compat() -> None:
    """Bridge transformers API drift for PaddleOCR-VL's remote modeling code.

    The model's remote code (saved with transformers 4.55) imports two helpers
    whose API changed in later transformers releases. Each shim below is installed
    on the source module so the remote module's `from transformers... import`
    picks it up, is signature-guarded to be a no-op when the installed API already
    matches, and is idempotent. Must be called before `from_pretrained` triggers
    the remote import.
    """
    _ensure_create_causal_mask_compat()
    _ensure_check_model_inputs_compat()


def _ensure_create_causal_mask_compat() -> None:
    """Alias `create_causal_mask(inputs_embeds=...)` to the renamed `input_embeds`.

    transformers >=4.56 renamed the keyword from `inputs_embeds` to `input_embeds`;
    PaddleOCR-VL's remote `Ernie4_5Model.forward` still passes `inputs_embeds`.
    No-op when the installed function still accepts `inputs_embeds`.
    """
    import functools
    import inspect

    import transformers.masking_utils as _masking

    current = getattr(_masking, "create_causal_mask", None)
    if current is None or getattr(current, "_paddle_vl_compat", False):
        return
    params = inspect.signature(current).parameters
    if "inputs_embeds" in params or "input_embeds" not in params:
        return

    @functools.wraps(current)
    def _wrapper(*args, _ccm=current, **kwargs):
        if "inputs_embeds" in kwargs and "input_embeds" not in kwargs:
            kwargs["input_embeds"] = kwargs.pop("inputs_embeds")
        return _ccm(*args, **kwargs)

    _wrapper._paddle_vl_compat = True
    _masking.create_causal_mask = _wrapper


def _ensure_check_model_inputs_compat() -> None:
    """Make the bare `@check_model_inputs` decorator work on transformers >=4.57.2.

    PaddleOCR-VL's remote code decorates `Ernie4_5Model.forward` with the
    pre-4.57.2 plain-decorator form. Newer transformers turned `check_model_inputs`
    into a decorator factory (`check_model_inputs(tie_last_hidden_states=True)`), so
    the bare usage would bind the forward function as the factory argument. Route a
    single callable positional argument to `factory()(func)`. No-op on builds that
    still expose the plain decorator.
    """
    import inspect

    import transformers.utils.generic as _generic

    current = getattr(_generic, "check_model_inputs", None)
    if current is None or getattr(current, "_paddle_vl_compat", False):
        return
    if "func" in inspect.signature(current).parameters:
        return

    def _compat(*args, _factory=current, **kwargs):
        if len(args) == 1 and not kwargs and callable(args[0]):
            return _factory()(args[0])
        return _factory(*args, **kwargs)

    _compat._paddle_vl_compat = True
    _generic.check_model_inputs = _compat


def _format_recognition_lines(
    text: str,
    *,
    join_newlines: bool,
    reflect_strings: bool,
) -> dict[str, Any]:
    """Split raw model text into trimmed non-empty lines and a joined string.

    `join_newlines=False` joins lines with spaces; `reflect_strings=True`
    reverses line order for right-to-left manga column reading.
    """
    lines = [
        line.strip()
        for line in str(text or "").replace("\r\n", "\n").split("\n")
        if line.strip()
    ]
    if reflect_strings:
        lines.reverse()

    output_text = "\n".join(lines) if join_newlines else " ".join(lines)
    return {
        "lines": lines,
        "text": output_text.strip(),
    }


def _resolve_selected_backend_device(fallback: str) -> str:
    """Resolve the configured `General.ai_device` to an available torch device."""
    fallback_norm = _normalize_backend_device(fallback, "cpu")
    configured = _read_configured_device()
    if configured is None:
        configured = fallback_norm

    normalized = _normalize_backend_device(configured, fallback_norm)
    available = _safe_available_devices()

    if normalized in available:
        return normalized
    if normalized.startswith("cuda") and "cuda" in available:
        return "cuda"
    if fallback_norm in available:
        return fallback_norm
    if "cuda" in available:
        return "cuda"
    return "cpu"


def _read_configured_device() -> str | None:
    config_root = getattr(UserConfig, "config", None)
    if not isinstance(config_root, dict):
        return None
    general = config_root.get("General")
    if not isinstance(general, dict):
        return None
    value = general.get("ai_device")
    if not isinstance(value, str):
        return None
    normalized = value.strip().lower()
    if normalized == "not-selected":
        return None
    return normalized or None


def _safe_available_devices() -> set[str]:
    try:
        return set(AIDevice.detect_available_devices())
    except Exception:
        return {"cpu"}


def _normalize_backend_device(raw: str, fallback: str) -> str:
    normalized = str(raw or "").strip().lower()
    if normalized == "cpu" or normalized == "cuda" or normalized.startswith("cuda:"):
        return normalized
    if normalized == "mps":
        return normalized
    return str(fallback or "cpu").strip().lower() or "cpu"
