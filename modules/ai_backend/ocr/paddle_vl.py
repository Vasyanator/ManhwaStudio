"""
FILE OVERVIEW: modules/ai_backend/ocr/paddle_vl.py
OCR service for PaddleOCR-VL run through the Hugging Face Transformers runtime
on VENDORED model code.

Main responsibilities:
- load one PaddleOCR-VL checkpoint variant offline from the absolute directory
  Rust sends (`paddle_vl_model` label + `paddle_vl_model_dir`), with the model,
  processor and image-processor classes of `paddle_vl_vendor/`;
- single-image text recognition from raw image bytes with a fixed OCR prompt
  (PaddleOCR-VL needs no separate text detection and no language selection);
- synchronization of the model device with backend `General.ai_device`;
- cooperation with `LoadedModelManager` (lease key per variant + device).

Key structures:
- `PaddleVlOcrService`

Key functions:
- `_load_model_and_processor()`: offline load + checkpoint-completeness guard
- `_load_vendored_classes()`: lazy import of `paddle_vl_vendor/`
- `_ensure_transformers_compat()`: API shims the vendored code needs

Notes:
- No `trust_remote_code` and no download: Rust (`ms_sysprobe::ai_models`)
  downloads and verifies the pinned checkpoint; this service reads only the
  directory it is given (`local_files_only=True`) and never the Hugging Face
  cache. A missing directory is an explicit error naming it. The backend holds
  no variant table: the label is opaque (lease key, health, logs).
- One vendored copy (PaddleOCR-VL-1.6 code) serves `official_1_6`,
  `official_1_5` and `manga_ja`; see `paddle_vl_vendor/MODULE_README.md`. A
  load whose loading info reports missing/unexpected/mismatched keys is refused,
  so a checkpoint that does not fit the code can never run on random weights.
- Generation runs with `use_cache=True`: every shipped `generation_config.json`
  says `use_cache: false`, which recomputes the full sequence per token.
- This runtime is PyTorch-only.
- The vendored modeling code imports two transformers helpers whose API changed
  after 4.55 (`create_causal_mask`'s `inputs_embeds` keyword became
  `input_embeds`; `check_model_inputs` became a decorator factory).
  `_ensure_transformers_compat()` installs signature-guarded aliases for those
  before the vendored module is imported, so the engine runs on the app's
  transformers 4.57.x without a global downgrade. The shims are no-ops when the
  installed transformers already matches.
- The checkpoint is stored in BF16 and is requested as BF16 on a bf16-capable
  GPU, so `from_pretrained` performs no host-side cast and hands out weights
  that still live in the safetensors file mapping. The host->device transfer
  therefore goes through `runtime.rocm_mmap_transfer.move_module_to`, which is a strict
  no-op off ROCm; see that module for the amdkfd stall it works around.
"""

from __future__ import annotations

import gc
import io
import logging
import re
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, NamedTuple

try:
    from ai_device import AIDevice
except Exception:
    from modules.ai_device import AIDevice

try:
    from config import UserConfig
except Exception:
    UserConfig = None

from ..runtime.model_manager import LoadedModelManager
from ..runtime.rocm_mmap_transfer import move_module_to
from .result_format import format_recognition_lines
from .script_constraint import ScriptConstraint, TokenByteIndex, normalize_script

log = logging.getLogger(__name__)

# Fixed prompt PaddleOCR-VL uses for plain text recognition (matches the
# official PaddleOCR-VL pipeline `text_prompt = "OCR:"`).
PADDLE_VL_OCR_PROMPT = "OCR:"
PADDLE_VL_MAX_NEW_TOKENS = 8192
# Safety cap for script-constrained mode: hard restriction can push the model
# into a non-terminating ramble on mismatched input, so bound the output length.
PADDLE_VL_CONSTRAINED_MAX_NEW_TOKENS = 1024
# Shape of the opaque `paddle_vl_model` label; it becomes part of a lease key.
_MODEL_LABEL_RE = re.compile(r"^[a-z0-9_]{1,64}$")
# Each checkpoint ships its own chat template (1.5/1.6 end the assistant turn
# with a newline, v1-based manga_ja with a space), so it is read per directory.
CHAT_TEMPLATE_FILE = "chat_template.jinja"
# Loading-info lists that must be empty for a checkpoint to fit the code.
_LOADING_INFO_KEYS = ("missing_keys", "unexpected_keys", "mismatched_keys", "error_msgs")


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


@dataclass(frozen=True)
class _ModelIdentity:
    """What a resident model was loaded for; any difference forces a reload."""

    model: str
    model_dir: Path
    device: str


class _VendoredClasses(NamedTuple):
    """The vendored classes `_load_model_and_processor` builds from."""

    model: Any
    processor: Any
    image_processor: Any


class PaddleVlOcrService:
    """PaddleOCR-VL OCR runtime backed by Hugging Face Transformers.

    One model (variant + directory + device) is resident at a time and shared
    across requests; `LoadedModelManager` may evict it when idle to keep the
    resident model count within the configured limit.
    """

    MODEL_KEY_PREFIX = "paddlevlocr"

    def __init__(self, model_manager: LoadedModelManager) -> None:
        self._lock = threading.Lock()
        self._model_manager = model_manager
        self._model = None
        self._processor = None
        self._identity: _ModelIdentity | None = None
        self._last_error: str | None = None
        # Lazily built once per loaded tokenizer; reused across script modes.
        self._token_index: TokenByteIndex | None = None
        self._constraints: dict[str, ScriptConstraint] = {}

    def health(self) -> dict[str, Any]:
        """Return `{"ready", "device", "model", "last_error"}` for the health snapshot.

        `model` echoes the resident variant label (None when nothing is loaded)
        so Rust can tell whether the ready model is the one it selected.
        """
        with self._lock:
            identity = self._identity
            return {
                "ready": self._model is not None,
                "device": identity.device if identity is not None else None,
                "model": identity.model if identity is not None else None,
                "last_error": self._last_error,
            }

    def recognize_image_bytes(
        self,
        image_bytes: bytes,
        *,
        model: str,
        model_dir: Path | str,
        join_newlines: bool = True,
        reflect_strings: bool = False,
        script: str | None = None,
    ) -> dict[str, Any]:
        """Recognize text in a single image and return `{"lines", "text"}`.

        `model` is the variant label (`^[a-z0-9_]{1,64}$`) and `model_dir` the
        absolute directory of that downloaded checkpoint. `join_newlines=False`
        collapses recognized lines with spaces instead of newlines;
        `reflect_strings=True` reverses line order for right-to-left manga
        column reading. `script` (`korean`/`chinese`/`japanese`, or None for
        auto) hard-restricts generation to that writing system plus whitespace,
        digits, and common punctuation. Raises RuntimeError for an invalid label
        or directory and when the model cannot load.
        """
        label = _validate_model_label(model)
        directory = _validate_model_dir(model_dir)
        image = self._decode_image(image_bytes)
        normalized_script = normalize_script(script)
        with self._lock:
            current_device = self._identity.device if self._identity is not None else None
        selected_device = _resolve_selected_backend_device(current_device or "cpu")
        identity = _ModelIdentity(model=label, model_dir=directory, device=selected_device)
        model_key = self._model_key(label, selected_device)

        def unload() -> bool:
            return self._unload_model_key(model_key)

        # Lease before the instance lock: an eviction callback of another
        # service may hold the manager while waiting for this lock.
        lease = self._model_manager.begin_model_use(model_key, unload_callback=unload)
        try:
            with self._lock:
                # Load scope: only a failure in here is a failed LOAD.
                try:
                    model_obj, processor = self._ensure_loaded_locked(
                        identity, lease_needs_load=lease.needs_load
                    )
                except Exception:
                    if lease.needs_load:
                        lease.mark_load_failed()
                    raise
            # Resident from here on, so it is registered before the constraint
            # build and inference: a failure there must leave it counted and
            # evictable, not occupying VRAM off the books.
            if lease.needs_load:
                lease.mark_loaded(unload_callback=unload)
            try:
                constraint = None
                if normalized_script is not None:
                    with self._lock:
                        constraint = self._constraint_for_script_locked(
                            processor, normalized_script
                        )
                text = self._generate_text(model_obj, processor, image, constraint)
            except Exception as exc:
                log.error(
                    "PaddleOCR-VL recognition failed: model=%s device=%s error=%s",
                    label,
                    selected_device,
                    exc,
                )
                with self._lock:
                    self._last_error = f"PaddleOCR-VL recognition failed: {exc}"
                raise
        finally:
            lease.release()

        with self._lock:
            self._last_error = None
        return format_recognition_lines(
            text,
            join_newlines=join_newlines,
            reflect_strings=reflect_strings,
        )

    def _constraint_for_script_locked(
        self, processor, script: str
    ) -> ScriptConstraint:
        """Return a `ScriptConstraint` for `script` over `processor`'s tokenizer.

        Cached per resident processor (the byte index scans the whole vocab).
        If another request replaced the resident model since `processor` was
        taken, an uncached constraint for `processor` is built instead, so a
        cache is never applied to a different tokenizer. Caller holds `self._lock`.
        """
        if processor is not self._processor:
            return ScriptConstraint(TokenByteIndex(processor.tokenizer), script)
        if self._token_index is None:
            self._token_index = TokenByteIndex(processor.tokenizer)
            self._constraints = {}
        constraint = self._constraints.get(script)
        if constraint is None:
            constraint = ScriptConstraint(self._token_index, script)
            self._constraints[script] = constraint
        return constraint

    @staticmethod
    def _generate_text(model, processor, image, constraint=None, *, use_cache: bool = True) -> str:
        """Run a single generate pass and decode only the newly generated tokens.

        `use_cache=True` (the production setting) reuses the K/V cache across
        steps; the checkpoints' own generation config disables it. When
        `constraint` is set, generation is hard-restricted to its writing system
        via a stateful UTF-8 `prefix_allowed_tokens_fn`."""
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

        generate_kwargs: dict[str, Any] = {
            "max_new_tokens": PADDLE_VL_MAX_NEW_TOKENS,
            "use_cache": use_cache,
        }
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

    def _ensure_loaded_locked(self, identity: _ModelIdentity, *, lease_needs_load: bool):
        """Load (or reuse) the model and processor for `identity`. Caller holds lock.

        A resident model with a different identity is dropped first. When its
        lease key differs from the new one, that key is reported unloaded
        (otherwise the manager would keep counting a model nobody holds). When
        the key is the same (only the directory changed) the entry stays
        resident, and a FAILED reload under a lease that found the key resident
        (`lease_needs_load=False`) reports it unloaded here, because that lease
        has no load to abort. Raises RuntimeError when loading fails.
        """
        if (
            self._model is not None
            and self._processor is not None
            and self._identity == identity
        ):
            return self._model, self._processor

        model_key = self._model_key(identity.model, identity.device)
        previous = self._identity
        if previous is not None:
            self._drop_model_locked()
            _clear_torch_cache()
            previous_key = self._model_key(previous.model, previous.device)
            if previous_key != model_key:
                self._model_manager.mark_unloaded(previous_key)
                log.info("PaddleOCR-VL model replaced: unloaded model_key=%s", previous_key)

        try:
            model, processor = _load_model_and_processor(identity.model_dir, identity.device)
        except Exception as exc:
            self._last_error = f"PaddleOCR-VL init failed: {exc}"
            log.error(
                "PaddleOCR-VL init failed: model=%s model_dir=%s device=%s error=%s",
                identity.model,
                identity.model_dir,
                identity.device,
                exc,
            )
            if not lease_needs_load:
                self._model_manager.mark_unloaded(model_key)
            raise RuntimeError(self._last_error) from exc

        self._model = model
        self._processor = processor
        self._identity = identity
        self._last_error = None
        log.info(
            "PaddleOCR-VL model ready: model=%s model_dir=%s device=%s",
            identity.model,
            identity.model_dir,
            identity.device,
        )
        return model, processor

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
        """Drop the model for `model_key`; `False` when another key is resident."""
        with self._lock:
            identity = self._identity
            if identity is None or model_key != self._model_key(identity.model, identity.device):
                return False
            self._drop_model_locked()
            _clear_torch_cache()
            self._model_manager.mark_unloaded(model_key)
        log.info("PaddleOCR-VL model unloaded: model_key=%s", model_key)
        return True

    def _drop_model_locked(self) -> None:
        self._model = None
        self._processor = None
        self._identity = None
        # Tokenizer/byte-index belongs to the dropped processor; rebuild on reload.
        self._token_index = None
        self._constraints = {}

    @classmethod
    def _model_key(cls, model: str, device: str) -> str:
        """Lease key `paddlevlocr:<variant label>:<device>`."""
        return f"{cls.MODEL_KEY_PREFIX}:{model}:{device}"


def _validate_model_label(raw: Any) -> str:
    """Return the `paddle_vl_model` label; RuntimeError unless it matches `^[a-z0-9_]{1,64}$`."""
    if not isinstance(raw, str) or _MODEL_LABEL_RE.fullmatch(raw) is None:
        raise RuntimeError(
            f"PaddleOCR-VL model label {raw!r} is invalid: expected 1-64 characters of [a-z0-9_]."
        )
    return raw


def _validate_model_dir(raw: Path | str) -> Path:
    """Return `raw` as a Path; RuntimeError naming it unless it is an absolute existing directory."""
    directory = Path(raw)
    if not directory.is_absolute():
        raise RuntimeError(f"PaddleOCR-VL model directory must be an absolute path: {directory}")
    if not directory.is_dir():
        raise RuntimeError(
            f"PaddleOCR-VL model directory does not exist: {directory}. "
            "Download the model in the text recognition panel first."
        )
    return directory


def _load_vendored_classes() -> _VendoredClasses:
    """Import the vendored PaddleOCR-VL classes (`paddle_vl_vendor/`).

    Must run after `_ensure_transformers_compat()`: the modeling module binds
    the shimmed transformers helpers at import time.
    """
    from .paddle_vl_vendor.image_processing_paddleocr_vl import PaddleOCRVLImageProcessor
    from .paddle_vl_vendor.modeling_paddleocr_vl import PaddleOCRVLForConditionalGeneration
    from .paddle_vl_vendor.processing_paddleocr_vl import PaddleOCRVLProcessor

    return _VendoredClasses(
        model=PaddleOCRVLForConditionalGeneration,
        processor=PaddleOCRVLProcessor,
        image_processor=PaddleOCRVLImageProcessor,
    )


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


def _require_complete_checkpoint(loading_info: dict[str, Any], model_dir: Path) -> None:
    """Raise RuntimeError when the checkpoint does not fill the vendored model exactly.

    transformers silently random-initializes missing parameters; a run on such
    a model would return garbage text instead of an error.
    """
    problems = []
    for key in _LOADING_INFO_KEYS:
        values = list(loading_info.get(key) or [])
        if values:
            problems.append(f"{key}={values[:5]}" + (" ..." if len(values) > 5 else ""))
    if problems:
        raise RuntimeError(
            f"PaddleOCR-VL checkpoint in {model_dir} does not match the vendored model code "
            f"({'; '.join(problems)}); refusing to run on randomly initialized weights."
        )


def _load_model_and_processor(model_dir: Path, device: str):
    """Load `(model, processor)` from `model_dir` offline with the vendored classes.

    Reads only `model_dir` (`local_files_only=True`; never the Hugging Face
    cache, never network). Raises RuntimeError for missing dependencies and an
    incomplete checkpoint; transformers' own errors propagate for unreadable
    files.
    """
    try:
        import torch  # type: ignore
        from transformers import AutoTokenizer
    except Exception as exc:
        raise RuntimeError(f"PaddleOCR-VL dependencies are not available: {exc}") from exc

    # Must run before the vendored modeling module is imported.
    _ensure_transformers_compat()
    classes = _load_vendored_classes()
    directory = str(model_dir)
    image_processor = classes.image_processor.from_pretrained(directory, local_files_only=True)
    tokenizer = AutoTokenizer.from_pretrained(directory, local_files_only=True)
    chat_template = (model_dir / CHAT_TEMPLATE_FILE).read_text(encoding="utf-8")
    processor = classes.processor(
        image_processor=image_processor,
        tokenizer=tokenizer,
        chat_template=chat_template,
    )
    model, loading_info = classes.model.from_pretrained(
        directory,
        dtype=_resolve_dtype(torch, device),
        local_files_only=True,
        output_loading_info=True,
    )
    _require_complete_checkpoint(loading_info, model_dir)
    # `dtype` above matches the checkpoint's own BF16 storage on a bf16 GPU, so
    # transformers skips the host-side cast and the parameters are still views
    # into the mmapped safetensors file. On ROCm such a source makes every
    # >=1 MiB host->device copy stall for seconds inside amdkfd, so the move is
    # routed through the staging helper (a plain `model.to(device)` elsewhere).
    model = move_module_to(model, device)
    model.eval()
    return model, processor


def _ensure_transformers_compat() -> None:
    """Bridge transformers API drift for PaddleOCR-VL's vendored modeling code.

    The vendored code (`paddle_vl_vendor/`, written against transformers 4.55)
    imports two helpers whose API changed in later transformers releases. Each
    shim below is installed on the source module so the vendored module's
    `from transformers... import` picks it up, is signature-guarded to be a no-op
    when the installed API already matches, and is idempotent. Must be called
    before `_load_vendored_classes()` imports the vendored modeling module.
    """
    _ensure_create_causal_mask_compat()
    _ensure_check_model_inputs_compat()


def _ensure_create_causal_mask_compat() -> None:
    """Alias `create_causal_mask(inputs_embeds=...)` to the renamed `input_embeds`.

    transformers >=4.56 renamed the keyword from `inputs_embeds` to `input_embeds`;
    PaddleOCR-VL's vendored `Ernie4_5Model.forward` still passes `inputs_embeds`.
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

    PaddleOCR-VL's vendored code decorates `Ernie4_5Model.forward` with the
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
