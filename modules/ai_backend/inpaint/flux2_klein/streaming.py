"""
File: modules/ai_backend/inpaint/flux2_klein/streaming.py

Purpose:
Load the single-file BFL klein transformer ONE TENSOR AT A TIME, so that the
host-memory peak is the size of the largest tensor instead of the size of the
whole checkpoint. diffusers 0.39 cannot do this: `load_single_file_checkpoint`
(`loaders/single_file_utils.py`) calls `load_state_dict()` on the WHOLE file
before anything moves to the accelerator, and `loaders/single_file_model.py`
then converts that whole dict before it calls `load_model_dict_into_meta`. On a
16.9 GiB checkpoint a sibling project measured the peak drop from 17.7 GiB to
1.55 GiB by streaming instead.

Main responsibilities:
- the streaming safetensors reader (`read_safetensors_header_and_data_start` from
  `components` for the header, `parse_tensor_spans` for the per-tensor contract,
  `StreamingSafetensorsReader` for the reads themselves);
- the per-key conversion driver (`checkpoint_layout`,
  `convert_tensor_to_diffusers`, `iter_converted_transformer_tensors`);
- the loader (`load_transformer_streaming`) and the precondition predicate the
  caller consults before choosing it (`streaming_load_eligible`,
  `_require_streaming_fill_contract`, `_require_single_file_checkpoint`,
  `_require_accelerator_target`);
- byte-level progress (`StreamingProgressCb`, `_ByteProgress`).

Key declarations:
- `TensorSpan` / `parse_tensor_spans` / `StreamingSafetensorsReader`
- `checkpoint_layout` / `convert_tensor_to_diffusers` /
  `iter_converted_transformer_tensors`
- `streaming_load_eligible` / `load_transformer_streaming`

Preconditions (the CALLER's to check, this module's to STATE and enforce):
this loader serves exactly one shape of request — a single `.safetensors` FILE,
`low_cpu_mem_usage=True`, a non-offload placement, and a CUDA (incl. ROCm/HIP)
target. Those four facts arrive as `low_cpu_mem_usage` plus the whole-model
`device_map` the caller already derived, and `streaming_load_eligible` answers
all four at once. Anything else is refused BY NAME. The accelerator target is
the one of the four that the LOADER does not enforce: the fill loop is correct
on any device (its `torch.cuda.synchronize` is guarded on the device type), and
"a CPU target gains nothing" is a question of worth, which belongs to the
predicate the caller consults — which is also what lets the tests exercise the
whole loader without a GPU. There is deliberately NO fallback to the ordinary
loader here: choosing the ordinary loader is the caller's decision and must be
logged where it is made.

Notes:
- This module sits directly after `components` in the package layering. It
  imports `components` and nothing else from the package — in particular not
  `progress`, whose `LOAD_STEP_*` numbering is a wire contract this module must
  not touch; the byte-level callback is a SECOND, independent progress channel.
- Why plain `read()` and not `safetensors.safe_open`: `safe_open` releases its
  pages only when the handle closes, so the peak still reaches the whole
  checkpoint. A `read()` into a `bytearray` plus `torch.frombuffer` leaves
  exactly one live tensor; the bytes that were read land in the page cache,
  which the kernel evicts and which is not part of the process's anonymous
  memory.
- Why NOT `rocm_mmap_transfer.patched_module_to()`: measured on this host,
  `torch.frombuffer(bytearray)` reports `tensor_needs_staging == False` (the
  buffer is anonymous memory, not the writable private file mapping that causes
  the amdkfd stall), the patch hooks `nn.Module.to` which
  `set_module_tensor_to_device` never calls, and that patch's contract forbids
  file I/O inside the block.
- torch, accelerate and diffusers are imported lazily inside the functions that
  need them, so the torch-free half of this module (header parsing, file order,
  refusals, eligibility) stays importable without them.
"""

from __future__ import annotations

import logging
import re
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Iterator

from .components import (
    _fp8_scaled_message,
    _missing_transformer_config_message,
    find_transformer_config_dir,
    is_fp8_scaled_checkpoint,
    read_safetensors_header,
    read_safetensors_header_and_data_start,
    validate_transformer_config_dir,
)

log = logging.getLogger(__name__)


# =====================================================================
#  The safetensors container, one tensor at a time
# =====================================================================
#: safetensors dtype token -> (torch dtype attribute name, bytes per element).
#: The 8-bit float tokens are deliberately ABSENT: a checkpoint carrying them is
#: an fp8_scaled one, which `components.is_fp8_scaled_checkpoint` recognizes from
#: the header and which this loader refuses with that module's wording — a much
#: better message than "unsupported dtype".
SAFETENSORS_DTYPES: dict[str, tuple[str, int]] = {
    "BOOL": ("bool", 1),
    "U8": ("uint8", 1),
    "I8": ("int8", 1),
    "I16": ("int16", 2),
    "U16": ("uint16", 2),
    "F16": ("float16", 2),
    "BF16": ("bfloat16", 2),
    "I32": ("int32", 4),
    "U32": ("uint32", 4),
    "F32": ("float32", 4),
    "I64": ("int64", 8),
    "U64": ("uint64", 8),
    "F64": ("float64", 8),
}

#: The key safetensors reserves for the container's own metadata; it describes no
#: tensor and carries no `data_offsets`.
_METADATA_KEY = "__metadata__"

#: Suffix a single-file transformer must have for this loader to read it.
_SAFETENSORS_SUFFIX = ".safetensors"

#: One part of a SHARDED checkout (`...-00001-of-00002.safetensors`). This loader
#: reads ONE whole file; a shard is refused by name instead of loading a fraction
#: of a model and failing on the missing-key check with a confusing count.
_SHARD_NAME_RE = re.compile(r"-\d{5}-of-\d{5}\.safetensors$")


@dataclass(frozen=True)
class TensorSpan:
    """Where one tensor lives in a safetensors file, and what it is.

    `start`/`end` are the header's own `data_offsets`, i.e. RELATIVE to the data
    section (`StreamingSafetensorsReader.data_start`), never absolute. `nbytes`
    is `end - start` and has already been checked against `shape` and the dtype's
    element size, so a reader may trust it.
    """

    name: str
    dtype_token: str
    torch_dtype_name: str
    shape: tuple[int, ...]
    start: int
    end: int

    @property
    def nbytes(self) -> int:
        """Length of this tensor's byte span in the file."""
        return self.end - self.start


def _span_error(source: Path, name: str, detail: str) -> ValueError:
    """One wording for every malformed tensor entry, so the file and key always appear."""
    return ValueError(f"Некорректная запись тензора «{name}» в {source}: {detail}")


def parse_tensor_spans(header: dict[str, Any], *, source: Path) -> list[TensorSpan]:
    """Validate every tensor entry of `header` and return the spans IN FILE ORDER.

    File order — sorted by `data_offsets[0]` — is not cosmetic: a klein
    checkpoint is ~17 GiB and may sit on a spinning disk, where reading in the
    header's (alphabetical) key order turns one sequential pass into random
    access across the whole file.

    Every entry is checked here rather than at read time, so a corrupt header
    fails before a single tensor byte is read: the dtype must be one this module
    can map, the shape must be non-negative integers, the offsets must be a
    non-negative ordered pair, the span must be non-empty, its length must equal
    `prod(shape) * itemsize`, and NO TWO SPANS MAY OVERLAP.

    Non-overlap is checked, full contiguity deliberately is NOT. Overlap is the
    one malformed layout that produces a silently WRONG result instead of an
    error: two expected weights aliasing the same bytes both land in `loaded`,
    `missing` stays empty, and the model runs on corrupted weights. A GAP between
    two spans cannot do that — this reader seeks absolutely to
    `data_start + span.start` for every tensor, so unreferenced bytes are simply
    never read — and demanding contiguity would refuse an alignment-padded
    container that this reader handles correctly.

    # Raises
    `ValueError` naming the file and the offending key; for an overlap, BOTH
    keys and the byte range they share.
    """
    spans: list[TensorSpan] = []
    for name, entry in header.items():
        if name == _METADATA_KEY:
            continue
        if not isinstance(entry, dict):
            raise _span_error(source, name, "запись не является объектом")

        token = str(entry.get("dtype", "")).upper()
        mapped = SAFETENSORS_DTYPES.get(token)
        if mapped is None:
            raise _span_error(
                source,
                name,
                f"неподдерживаемый тип данных «{entry.get('dtype')}» "
                f"(известные: {', '.join(sorted(SAFETENSORS_DTYPES))})",
            )
        torch_dtype_name, item_bytes = mapped

        raw_shape = entry.get("shape")
        if not isinstance(raw_shape, list) or not all(
            isinstance(dim, int) and not isinstance(dim, bool) and dim >= 0 for dim in raw_shape
        ):
            raise _span_error(source, name, f"некорректная форма {raw_shape!r}")
        shape = tuple(int(dim) for dim in raw_shape)

        offsets = entry.get("data_offsets")
        if not isinstance(offsets, list) or len(offsets) != 2 or not all(
            isinstance(value, int) and not isinstance(value, bool) for value in offsets
        ):
            raise _span_error(source, name, f"некорректные data_offsets {offsets!r}")
        start, end = int(offsets[0]), int(offsets[1])
        if start < 0 or end < start:
            raise _span_error(source, name, f"смещения идут вспять: {start}..{end}")
        if end == start:
            # Not skipped silently: an empty span means the file does not carry
            # this weight, and substituting an empty tensor would let the model
            # build with a hole in it.
            raise _span_error(source, name, "нулевая длина данных")

        elements = 1
        for dim in shape:
            elements *= dim
        declared = elements * item_bytes
        if declared != end - start:
            raise _span_error(
                source,
                name,
                f"длина данных {end - start} B не совпадает с формой {shape} "
                f"типа {token} ({declared} B)",
            )

        spans.append(
            TensorSpan(
                name=name,
                dtype_token=token,
                torch_dtype_name=torch_dtype_name,
                shape=shape,
                start=start,
                end=end,
            )
        )

    spans.sort(key=lambda span: span.start)
    # Pairwise once the spans are ordered: with `start` ascending, an overlap can
    # only be with the immediate predecessor, so one pass is enough.
    for previous, current in zip(spans, spans[1:]):
        if current.start < previous.end:
            raise ValueError(
                f"Перекрывающиеся тензоры в {source}: «{previous.name}» занимает байты "
                f"{previous.start}..{previous.end}, а «{current.name}» — "
                f"{current.start}..{current.end}. Такой файл повреждён: один и тот же "
                "участок данных прочитался бы как два разных веса."
            )
    return spans


class StreamingSafetensorsReader:
    """Reads a safetensors file tensor by tensor with ordinary `read()` calls.

    Construction parses and fully validates the header (`parse_tensor_spans`) and
    checks every declared span against the file's real size, so `read_tensor` can
    fail only on I/O. Reads must happen inside the context manager; the file
    handle is unbuffered, because every read is already one whole tensor.

    Each returned tensor VIEWS its own freshly allocated `bytearray`, so exactly
    one tensor is alive in anonymous memory at a time — that is the whole point
    of this class, and it is why `safetensors.safe_open` is not used (its pages
    are released only when the handle closes, so the peak still reaches the whole
    checkpoint).
    """

    def __init__(self, path: Path | str) -> None:
        self.path = Path(path)
        self.header, self.data_start = read_safetensors_header_and_data_start(self.path)
        try:
            self.file_size = int(self.path.stat().st_size)
        except OSError as exc:
            raise ValueError(f"Не удалось получить размер {self.path}: {exc}") from exc
        self.spans = parse_tensor_spans(self.header, source=self.path)
        for span in self.spans:
            if self.data_start + span.end > self.file_size:
                raise ValueError(
                    f"Обрезанный safetensors {self.path}: тензор «{span.name}» объявлен до байта "
                    f"{self.data_start + span.end}, а файл занимает {self.file_size} B"
                )
        self._handle: Any = None

    def __enter__(self) -> StreamingSafetensorsReader:
        self._handle = self.path.open("rb", buffering=0)
        return self

    def __exit__(self, *exc_info: object) -> None:
        if self._handle is not None:
            self._handle.close()
            self._handle = None

    @property
    def total_tensor_bytes(self) -> int:
        """Sum of every tensor's span — the denominator byte progress is reported against.

        Deliberately not the file size: the header and any padding are not
        tensors, so a fraction taken against `st_size` never reaches 1.0.
        """
        return sum(span.nbytes for span in self.spans)

    def keys_in_file_order(self) -> list[str]:
        """Tensor names ordered by their offset in the file. See `parse_tensor_spans`."""
        return [span.name for span in self.spans]

    def read_tensor(self, span: TensorSpan) -> Any:
        """Read one tensor's bytes and wrap them, without a copy, in a `torch.Tensor`.

        The result owns a `bytearray` of exactly `span.nbytes`; nothing else of
        the file stays in anonymous memory.

        # Raises
        `RuntimeError` when called outside the context manager; `ValueError` when
        the file ends inside the tensor or the read fails.
        """
        import torch

        if self._handle is None:
            raise RuntimeError(
                f"StreamingSafetensorsReader({self.path}) используется вне контекстного менеджера"
            )
        length = span.nbytes
        buffer = bytearray(length)
        view = memoryview(buffer)
        try:
            self._handle.seek(self.data_start + span.start)
            read = 0
            while read < length:
                got = self._handle.readinto(view[read:])
                if not got:
                    raise ValueError(
                        f"Файл {self.path} оборвался на тензоре «{span.name}»: прочитано "
                        f"{read} B из {length} B"
                    )
                read += got
        except OSError as exc:
            raise ValueError(f"Ошибка чтения тензора «{span.name}» из {self.path}: {exc}") from exc
        tensor = torch.frombuffer(buffer, dtype=getattr(torch, span.torch_dtype_name))
        return tensor.reshape(span.shape)


# =====================================================================
#  Which layout the checkpoint is in — decided ONCE, from the header
# =====================================================================
#: The checkpoint carries BFL (black-forest-labs) key names and must go through
#: `convert_flux2_transformer_checkpoint_to_diffusers`.
CHECKPOINT_LAYOUT_BFL = "bfl"

#: The checkpoint already carries diffusers key names; the converter must NOT run.
CHECKPOINT_LAYOUT_DIFFUSERS = "diffusers"

#: The key diffusers 0.39 itself uses to recognize a FLUX.2 checkpoint
#: (`single_file_utils.CHECKPOINT_KEY_NAMES["flux2"]`). Present in every BFL
#: klein file and in no diffusers-layout one.
FLUX2_BFL_MARKER_KEY = "single_stream_modulation.lin.weight"

#: Prefix some redistributed checkpoints keep in front of every key. diffusers
#: 0.39's flux2 converter does NOT strip it (verified: the prefixed marker key
#: converts to `model.diffusion_model.single_stream_modulation.linear.weight`,
#: which matches no model key), so such a file gets its own refusal instead of a
#: "weights are missing" report naming every tensor in the model.
_LDM_UNET_PREFIX = "model.diffusion_model."


def checkpoint_layout(
    checkpoint_keys: Iterable[str], model_keys: Iterable[str], *, source: Path
) -> str:
    """Decide, ONCE and from the key sets alone, whether the converter must run.

    This mirrors the single cross-key decision diffusers makes upstream
    (`single_file_model._should_convert_state_dict_to_diffusers`), which compares
    the WHOLE checkpoint key set against `model.state_dict()`. It cannot be taken
    per tensor, and it must not be assumed: an already-diffusers-layout file does
    happen to survive the converter unchanged, but relying on that is a silent
    fallback. A checkpoint matching neither shape is refused here rather than
    reported later as a wall of missing keys.

    `checkpoint_keys` are the RAW header keys; `model_keys` are
    `model.state_dict()` keys of the skeleton built from the config.

    # Raises
    `ValueError` when the checkpoint is neither BFL nor diffusers layout.
    """
    checkpoint = set(checkpoint_keys)
    expected = set(model_keys)
    if checkpoint == expected:
        return CHECKPOINT_LAYOUT_DIFFUSERS
    if FLUX2_BFL_MARKER_KEY in checkpoint:
        return CHECKPOINT_LAYOUT_BFL
    if _LDM_UNET_PREFIX + FLUX2_BFL_MARKER_KEY in checkpoint:
        raise ValueError(
            f"Чекпоинт {source.name} хранит веса с приставкой «{_LDM_UNET_PREFIX}» в каждом ключе. "
            "Конвертер FLUX.2 в diffusers 0.39 её не убирает, поэтому такой файл не загрузить — "
            "возьмите чекпоинт трансформера без этой приставки."
        )
    sample = ", ".join(sorted(checkpoint)[:5])
    raise ValueError(
        f"Чекпоинт {source.name} не похож ни на формат BFL FLUX.2 (нет ключа "
        f"«{FLUX2_BFL_MARKER_KEY}»), ни на формат diffusers (набор из {len(checkpoint)} ключей не "
        f"совпадает с {len(expected)} ключами модели). Первые ключи файла: {sample}"
    )


def convert_tensor_to_diffusers(
    name: str, tensor: Any, *, layout: str, config: dict[str, Any]
) -> dict[str, Any]:
    """Convert ONE checkpoint tensor into its diffusers key(s).

    Under `CHECKPOINT_LAYOUT_DIFFUSERS` this is the identity. Under
    `CHECKPOINT_LAYOUT_BFL` it calls diffusers' own
    `convert_flux2_transformer_checkpoint_to_diffusers` on a ONE-KEY dict, which
    is sound because that converter is per-key: it renames by substring and its
    three special handlers (`adaLN_modulation`, `double_blocks`, `single_blocks`)
    each look at their own key only. The two splits it performs —
    `torch.chunk` of a fused qkv and `swap_scale_shift` of an adaLN weight — are
    WITHIN one tensor, so one key may yield several. Verified on nine real BFL
    keys: nine one-key calls produce the same key set and the same values as one
    full-dict call, and `test_streaming.py` defends that equality. The key
    mapping itself must never be reimplemented here.

    `config` is forwarded to the converter's `config=` kwarg for signature
    fidelity only — diffusers 0.39 never reads it.

    Returns a dict of diffusers key -> tensor (usually one entry, three for a
    fused qkv).
    """
    if layout == CHECKPOINT_LAYOUT_DIFFUSERS:
        return {name: tensor}
    if layout != CHECKPOINT_LAYOUT_BFL:
        raise ValueError(f"Неизвестный формат чекпоинта: {layout!r}")

    from diffusers.loaders.single_file_utils import (
        convert_flux2_transformer_checkpoint_to_diffusers,
    )

    # The converter POPS every key of the dict it is given, so it must be a fresh
    # one-key dict and never the caller's own mapping.
    return convert_flux2_transformer_checkpoint_to_diffusers({name: tensor}, config=config)


def iter_converted_transformer_tensors(
    reader: StreamingSafetensorsReader, *, layout: str, config: dict[str, Any]
) -> Iterator[tuple[dict[str, Any], TensorSpan]]:
    """Stream `(diffusers tensors, source span)` pairs in file order.

    The reader must already be inside its context manager. Exactly one source
    tensor is alive per iteration: the caller consumes and drops each dict, and
    the `del` after the `yield` drops this frame's own reference to it before the
    next tensor is read — without it the generator would keep tensor N alive
    while tensor N+1 is being read, doubling the peak this module exists to cut.
    """
    for span in reader.spans:
        converted = convert_tensor_to_diffusers(
            span.name, reader.read_tensor(span), layout=layout, config=config
        )
        yield converted, span
        del converted


# =====================================================================
#  Byte-level progress
# =====================================================================
#: Byte-level progress callback: `(done_bytes, total_bytes, last_key)`.
#: A SECOND progress channel next to `progress.LOAD_STEP_*`, which stays a
#: step-level wire contract this module does not touch.
StreamingProgressCb = Callable[[int, int, str], None]

#: Smallest gap between two progress reports. A klein checkpoint holds thousands
#: of tensors and reporting each one would be thousands of frames per load.
PROGRESS_MIN_INTERVAL_SECONDS = 2.0


class _ByteProgress:
    """Rate-limits a `StreamingProgressCb` and never lets it break the load.

    `advance` is called once per tensor; it reports at most every
    `interval_seconds`, plus unconditionally when `final=True`, so the bar always
    ends at 100%. A callback that raises is logged at debug and swallowed — a
    dead IPC peer must not kill a load that is otherwise succeeding.
    """

    def __init__(
        self,
        callback: StreamingProgressCb | None,
        total_bytes: int,
        *,
        interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._callback = callback
        self._total = int(total_bytes)
        self._interval = float(interval_seconds)
        self._clock = clock
        self._done = 0
        self._last_report = clock()

    @property
    def done_bytes(self) -> int:
        """Bytes of the checkpoint consumed so far."""
        return self._done

    def advance(self, nbytes: int, key: str, *, final: bool = False) -> None:
        """Count `nbytes` against the total and report if the interval has elapsed."""
        self._done += int(nbytes)
        if self._callback is None:
            return
        now = self._clock()
        if not final and now - self._last_report < self._interval:
            return
        self._last_report = now
        try:
            self._callback(self._done, self._total, key)
        except Exception as exc:  # noqa: BLE001 - a dead peer must not kill the load
            log.debug("FLUX.2 klein: обработчик прогресса загрузки бросил исключение: %s", exc)


# =====================================================================
#  Preconditions
# =====================================================================
def _require_streaming_fill_contract(
    *, low_cpu_mem_usage: bool, device_map: dict[str, Any] | None
) -> str:
    """Check the two facts the FILL loop cannot work without, and return the target.

    Both are read off the caller's OWN derivation rather than re-derived here:
    the service builds a whole-model `device_map` exactly when
    `low_cpu_mem_usage` is set and the placement is a non-offload one, so a
    `device_map` of the shape `{"": "cuda:0"}` IS "non-offload placement onto a
    named device". Nothing in this module therefore repeats the list of
    placement names.

    The returned string is the device every parameter will be written to. It is
    NOT required to be an accelerator: the loop is correct on any device, and the
    `torch.cuda.synchronize` it needs is guarded on the device type. Whether
    streaming is WORTH choosing — which is where the accelerator target belongs —
    is `streaming_load_eligible`'s question, not this one's.

    # Raises
    `ValueError` naming the precondition that failed.
    """
    if not low_cpu_mem_usage:
        raise ValueError(
            "Потоковая загрузка трансформера работает только при low_cpu_mem_usage=True: "
            "она собирает модель на meta-устройстве и заполняет её тензор за тензором."
        )
    if not device_map:
        raise ValueError(
            "Потоковая загрузка трансформера требует размещения модели целиком (device_map на "
            "всю модель), а текущий режим отдаёт размещение accelerate."
        )
    if set(device_map) != {""}:
        raise ValueError(
            f"Потоковая загрузка трансформера принимает только device_map на всю модель "
            f"({{'': 'cuda:0'}}), получено: {sorted(device_map)}"
        )
    return str(device_map[""])


def _require_single_file_checkpoint(source: Path | str) -> Path:
    """Check that `source` is the ONE `.safetensors` file this reader can stream.

    # Raises
    `ValueError` naming why the input is not one: a diffusers directory, a
    missing path, another container format, or one part of a sharded checkout.
    """
    path = Path(source)
    if path.is_dir():
        raise ValueError(
            f"Потоковая загрузка читает ОДИН файл .safetensors, а {path} — каталог diffusers; "
            "его грузит обычный загрузчик."
        )
    if not path.is_file():
        raise ValueError(f"Файл трансформера не найден: {path}")
    if path.suffix.lower() != _SAFETENSORS_SUFFIX:
        raise ValueError(
            f"Потоковая загрузка читает только контейнеры {_SAFETENSORS_SUFFIX}, "
            f"а получен файл {path.name}."
        )
    if _SHARD_NAME_RE.search(path.name):
        raise ValueError(
            f"Файл {path.name} — одна часть шардированного чекпоинта; потоковая загрузка читает "
            "чекпоинт целиком одним файлом."
        )
    return path


def _require_accelerator_target(target: str) -> None:
    """Refuse a target that streaming would not BUY anything on.

    Streaming exists to keep the host-memory peak at one tensor while the weights
    go somewhere else. Filling a CPU-resident model from a CPU-read file gains
    nothing over the ordinary loader and gives up its optimizations, so an
    accelerator target is part of ELIGIBILITY — not of the loader's own contract,
    which is device-agnostic on purpose.

    # Raises
    `ValueError` naming the device.
    """
    if target.split(":", 1)[0] != "cuda":
        raise ValueError(
            f"Потоковая загрузка имеет смысл только при размещении на CUDA/ROCm, "
            f"а размещение указывает на «{target}»."
        )


def streaming_load_eligible(
    source: Path | str, *, low_cpu_mem_usage: bool, device_map: dict[str, Any] | None
) -> tuple[bool, str]:
    """Whether `load_transformer_streaming` should be used for this request.

    Returns `(True, "")` when it should, and `(False, reason)` when it should not
    — `reason` is the ready-to-log Russian sentence naming the precondition that
    failed. This is the union of the loader's own contract
    (`_require_streaming_fill_contract`, `_require_single_file_checkpoint`) and
    the one condition that is about WORTH rather than correctness
    (`_require_accelerator_target`).

    The caller decides what to do with a `False`; this module never falls back to
    the ordinary loader itself, and that choice must be logged where it is made.
    """
    try:
        target = _require_streaming_fill_contract(
            low_cpu_mem_usage=low_cpu_mem_usage, device_map=device_map
        )
        _require_accelerator_target(target)
        _require_single_file_checkpoint(source)
    except (ValueError, OSError) as exc:
        return False, str(exc)
    return True, ""


def _require_plain_loading_contract(model_cls: Any) -> None:
    """Refuse a model class whose loading needs machinery this loader does not run.

    `from_single_file` honours `_keep_in_fp32_modules` (a per-module dtype
    override) and `_keys_to_ignore_on_load_unexpected`. `Flux2Transformer2DModel`
    declares NEITHER in diffusers 0.39 — asserted here rather than assumed, so a
    diffusers upgrade that adds one is a refusal instead of a model quietly
    loaded in the wrong dtype.

    # Raises
    `ValueError` naming the class and the attribute.
    """
    for attribute in ("_keep_in_fp32_modules", "_keys_to_ignore_on_load_unexpected"):
        value = getattr(model_cls, attribute, None)
        if value is not None:
            raise ValueError(
                f"{getattr(model_cls, '__name__', model_cls)} объявляет {attribute}={value!r}; "
                "потоковая загрузка этого не поддерживает — используйте обычный загрузчик."
            )


# =====================================================================
#  The loader
# =====================================================================
def _set_buffer(model: Any, dotted_name: str, value: Any) -> None:
    """Replace the buffer `dotted_name` of `model` with `value` in place."""
    parent = model
    *path, leaf = dotted_name.split(".")
    for part in path:
        parent = getattr(parent, part)
    setattr(parent, leaf, value)


def load_transformer_streaming(
    model_cls: Any,
    source: Path | str,
    *,
    dtype: Any,
    device_map: dict[str, Any] | None,
    low_cpu_mem_usage: bool,
    progress: StreamingProgressCb | None = None,
    progress_interval_seconds: float = PROGRESS_MIN_INTERVAL_SECONDS,
) -> Any:
    """Load a single-file BFL klein transformer straight onto the accelerator.

    The host-memory peak is one tensor instead of the whole checkpoint: the model
    skeleton is built under `accelerate.init_empty_weights()`, and each tensor is
    read, converted and handed to `load_model_dict_into_meta` on its own.

    `source` must be one `.safetensors` FILE (`_require_single_file_checkpoint`),
    `low_cpu_mem_usage` must be `True` and `device_map` must be a whole-model map
    (`_require_streaming_fill_contract`); consult `streaming_load_eligible`
    BEFORE calling this, since it additionally answers whether streaming is worth
    choosing at all. The transformer's own `config.json` must be next to the
    checkpoint (`components.find_transformer_config_dir`): there is no Hub
    fallback and no config reconstructed from the weights, for the reasons
    `_missing_transformer_config_message` states. `guidance_embeds=False` is
    forced into the config here, because klein has no guidance embedder and this
    path — unlike `from_single_file`, which merges model kwargs into the resolved
    config — inherits nothing.

    `progress` is a BYTE-level callback, rate-limited to
    `progress_interval_seconds`; it is independent of the step-level
    `progress.LOAD_STEP_*` protocol.

    The load is verified before it returns: every key of `model.state_dict()` must
    have been filled (otherwise the checkpoint is not klein, or not this
    variant), no parameter may be left on `meta`, buffers `init_empty_weights`
    left on the host are re-homed onto the target device, and the model is put in
    `eval()` mode. Keys the model does not declare are logged, never raised.

    # Raises
    `ValueError` for a violated precondition, an fp8_scaled checkpoint, a corrupt
    container or a checkpoint in an unknown layout; `FileNotFoundError` when the
    transformer config is missing; `RuntimeError` when weights are left
    unfilled or on `meta`.
    """
    import torch
    from accelerate import init_empty_weights
    from diffusers.models.model_loading_utils import load_model_dict_into_meta

    target = _require_streaming_fill_contract(
        low_cpu_mem_usage=low_cpu_mem_usage, device_map=device_map
    )
    path = _require_single_file_checkpoint(source)
    device = torch.device(target)

    # The fp8 refusal must come BEFORE the spans are parsed: an fp8 checkpoint's
    # dtype token is not in `SAFETENSORS_DTYPES` on purpose, so `parse_tensor_spans`
    # would otherwise refuse it with "unsupported dtype" instead of the wording
    # that tells the user what to do about it. Parsing the header twice costs a
    # few hundred KB next to a multi-GiB checkpoint.
    if is_fp8_scaled_checkpoint(read_safetensors_header(path)):
        raise ValueError(_fp8_scaled_message(path))
    reader = StreamingSafetensorsReader(path)

    config_dir = find_transformer_config_dir(path)
    if config_dir is None:
        raise FileNotFoundError(_missing_transformer_config_message(path))
    validate_transformer_config_dir(config_dir)
    _require_plain_loading_contract(model_cls)

    started = time.perf_counter()
    config = dict(model_cls.load_config(str(config_dir), local_files_only=True))
    # klein has no `guidance_in` block. `from_single_file` would merge this in
    # through its model kwargs; `from_config` inherits nothing, so it is set here
    # or the wrong architecture is built.
    config["guidance_embeds"] = False
    with init_empty_weights():
        model = model_cls.from_config(config)

    expected = set(model.state_dict())
    layout = checkpoint_layout(reader.keys_in_file_order(), expected, source=path)
    total_bytes = reader.total_tensor_bytes
    log.info(
        "FLUX.2 klein: потоковая загрузка трансформера %s (%d тензоров, %.2f ГиБ, формат %s) "
        "напрямую на %s, конфиг из %s",
        path.name,
        len(reader.spans),
        total_bytes / (1024**3),
        layout,
        device,
        config_dir,
    )

    # `torch.cuda.synchronize` after EVERY tensor is mandatory on the accelerator
    # path: `load_model_dict_into_meta` copies with `non_blocking=True`, and HIP
    # stages non-pinned host memory through its own buffers, which otherwise
    # accumulate until the peak equals the whole checkpoint (measured elsewhere:
    # 17.6 GiB without it). A CPU target has nothing to synchronize.
    synchronize = device.type == "cuda"
    reporter = _ByteProgress(
        progress, total_bytes, interval_seconds=progress_interval_seconds
    )

    loaded: set[str] = set()
    unexpected_seen: list[str] = []
    with reader:
        last_index = len(reader.spans) - 1
        for index, (converted, span) in enumerate(
            iter_converted_transformer_tensors(reader, layout=layout, config=config)
        ):
            unexpected = [key for key in converted if key not in expected]
            if unexpected:
                unexpected_seen.extend(unexpected)
            load_model_dict_into_meta(
                model,
                converted,
                dtype=dtype,
                device_map={"": device},
                unexpected_keys=unexpected,
            )
            loaded.update(key for key in converted if key in expected)
            del converted
            if synchronize:
                torch.cuda.synchronize(device)
            reporter.advance(span.nbytes, span.name, final=index == last_index)

    missing = sorted(expected - loaded)
    if missing:
        raise RuntimeError(
            f"После чтения {path.name} остались незаполненные веса ({len(missing)} шт.), "
            f"первые: {', '.join(missing[:5])}. Похоже, это не чекпоинт FLUX.2 klein или не тот "
            "его вариант, что описан лежащим рядом config.json."
        )
    if unexpected_seen:
        log.warning(
            "FLUX.2 klein: в чекпоинте %s %d лишних ключей, они пропущены; первые: %s",
            path.name,
            len(unexpected_seen),
            ", ".join(unexpected_seen[:5]),
        )

    # `init_empty_weights` does not touch buffers, so they are still on the host
    # while every parameter is already on the device.
    for name, buffer in list(model.named_buffers()):
        if buffer.device != device:
            _set_buffer(model, name, buffer.to(device=device))
    stray = [name for name, parameter in model.named_parameters() if parameter.is_meta]
    if stray:
        raise RuntimeError(
            f"После загрузки {path.name} параметры остались на meta-устройстве "
            f"({len(stray)} шт.), первые: {', '.join(stray[:5])}."
        )
    model.eval()
    log.info(
        "FLUX.2 klein: трансформер загружен потоково за %.1f с (%.2f ГиБ)",
        time.perf_counter() - started,
        total_bytes / (1024**3),
    )
    return model
