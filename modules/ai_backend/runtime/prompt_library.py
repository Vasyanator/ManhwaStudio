"""
File: modules/ai_backend/runtime/prompt_library.py

Purpose:
The model-agnostic prompt-embedding LIBRARY: one directory per model
(`<root>/<library id>/`), one `.msprompt` file per cached embedding (container
v2, a safetensors file whose `__metadata__` is the source of truth), a
rebuildable `index.json` beside them, and THE rule that picks which entry
serves a prompt (`select_entry`). A model package (FLUX.2 klein today) owns
everything model-specific: its library ids, how it identifies its text
encoder, which tensor width it accepts, and what it does with the tensor.

Main responsibilities:
- the container v2 metadata (`build_metadata`, `parse_entry_header`) and its
  header-only rewrite (`rewrite_safetensors_metadata`);
- deterministic entry file names (`entry_file_name`) and preset names
  (`normalize_preset_name` over the explicit `NAME_REMOVED_RANGES` table the
  Rust client mirrors, `default_preset_name`);
- the selection rule (`Preference`, `rank_key`, `select_entry`);
- `PromptLibrary`: lazy load + reconcile against the files, the published
  immutable `LibrarySnapshot`, entry writes, payload adds (import / migration),
  preset renames, validated tensor reads (`LibraryEntryGone`);
- `library_for`: the process-wide registry (one instance, one lock per
  directory);
- `publish_bytes_atomically` (staged, fsynced, replaced, directory fsynced)
  and the client-path validation of export/import (`require_entry_destination`,
  `require_entry_source`);
- the containment rule of every rewrite, replace and delete
  (`require_confined`, `require_real_directory`, `remove_confined_file`,
  `remove_confined_empty_directory`, `UnsafeLibraryPath`) and directory
  durability (`sync_directory`).

Key structures:
- EncoderProvenance, LibraryEntry, LibrarySpec, Preference, LibrarySnapshot

Notes:
- Stdlib only at import time; `safetensors.torch` is imported lazily by
  `serialize_tensor` / `load_tensor`, the two patch points the tests replace.
- Files are the truth, the index is a cache: a lost, malformed, foreign or
  newer-version index is rebuilt from the files (logged), never repaired. Two
  backend processes writing one library can race on `index.json`; the next
  reconcile heals it because it re-reads any file whose (size, mtime) differs.
- The library never deletes an entry on its own: a corrupt or foreign file is
  reported in `skipped` and left where it is.
- No symbolic link (or Windows reparse point) is ever followed for a rewrite,
  replace or delete: a linked `.msprompt` is `skipped` (`LINK_SKIP_REASON`), a
  linked library or root directory is neither listed nor written. The v1
  migration (`inpaint/flux2_klein/prompt_cache.py`) uses the same helpers.
"""

from __future__ import annotations

import calendar
import json
import logging
import os
import re
import struct
import stat
import threading
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from hashlib import sha256
from pathlib import Path
from types import MappingProxyType
from typing import Any, TypeVar

from .safetensors_stream import read_safetensors_header

log = logging.getLogger(__name__)

T = TypeVar("T")

# ---------------------------------------------------------------------------
# Contract constants
# ---------------------------------------------------------------------------
#: `__metadata__["format"]` of a container v2 entry. Generic: every model's
#: library uses it, its `library` field says which one.
CONTAINER_FORMAT = "manhwastudio.prompt_cache"

#: Container version. A file of a NEWER version is skipped (never read with
#: today's rules); version 1 was FLUX.2's own format and is read only by its
#: migration and import (`inpaint/flux2_klein/prompt_cache.py`).
CONTAINER_VERSION = 2

#: File suffix of an entry. Checked on every client-supplied path.
ENTRY_SUFFIX = ".msprompt"

#: The rebuildable index beside the entries.
INDEX_FILE = "index.json"
INDEX_FORMAT = "manhwastudio.prompt_cache.index"
INDEX_VERSION = 1

#: Shape of a library id: it is a directory name and a persistence key.
LIBRARY_ID_PATTERN = re.compile(r"^[a-z0-9_]{1,40}$")

#: Shape of an encoder source id (`""` = unknown source) and of a format id.
SOURCE_ID_PATTERN = re.compile(r"^[a-z0-9_]{0,32}$")
FORMAT_ID_PATTERN = re.compile(r"^[a-z0-9_]{1,32}$")

#: Shape of an origin id (`built`, `imported`, `migrated_v1`, ...). Unknown
#: origins of a newer build are kept as they are.
ORIGIN_ID_PATTERN = re.compile(r"^[a-z0-9_]{1,40}$")

#: Origins this build writes.
ORIGIN_BUILT = "built"
ORIGIN_IMPORTED = "imported"
ORIGIN_MIGRATED_V1 = "migrated_v1"
ORIGIN_MIGRATED_V1_UNVERIFIED = "migrated_v1_unverified"

#: Precision id of an entry encoded with weight-only fp8 (`EncoderProvenance.precision_id`).
FP8_PRECISION = "fp8"

#: Longest preset name kept, in characters.
MAX_PRESET_NAME_CHARS = 100

#: Characters of the prompt in a derived default preset name and in a file slug.
DEFAULT_NAME_CHARS = 40
SLUG_CHARS = 40

#: Hex characters of `sha256(prompt + "\\0" + msl)` and of the encoder id in a file name.
TEXT_HASH_CHARS = 10
ENCODER_ID_CHARS = 8

#: `created_at` layout (UTC).
CREATED_AT_FORMAT = "%Y-%m-%dT%H:%M:%SZ"

#: Metadata keys the library owns; `extra` may not override them.
RESERVED_METADATA_KEYS = frozenset(
    {
        "format",
        "format_version",
        "library",
        "name",
        "prompt",
        "max_sequence_length",
        "dtype",
        "encoder_source",
        "encoder_format",
        "encoder_fp8",
        "encoder_id",
        "encoder_label",
        "created_at",
        "origin",
    }
)

#: THE preset-name normalization table (inclusive code-point ranges replaced by
#: a space): every control character (Cc), the format characters in practical
#: use (soft hyphen, Arabic / Syriac marks, Mongolian vowel separator,
#: zero-width and bidi controls, invisible operators, BOM, interlinear
#: annotation, the tag block) and the private-use areas. An explicit table, not
#: a Unicode category lookup, because the Rust client mirrors it exactly
#: (`shared/prompt_library.rs` `NAME_REMOVED_RANGES`) and `std` has no category
#: data; `fixtures/preset_name_normalization.json` carries the same ranges and
#: both sides' tests compare against it.
NAME_REMOVED_RANGES: tuple[tuple[int, int], ...] = (
    (0x0000, 0x001F),
    (0x007F, 0x009F),
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xE000, 0xF8FF),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xF0000, 0x10FFFF),
)

#: Longest encoder label kept.
_MAX_LABEL_CHARS = 200

#: Upper bound on a container header parsed from memory (an entry header is a few KiB).
_MAX_PAYLOAD_HEADER_BYTES = 1024 * 1024


#: The `skipped` reason of a `.msprompt` name that is a symbolic link (never read, never replaced).
LINK_SKIP_REASON = "символьная ссылка: ManhwaStudio не читает и не заменяет файлы через ссылки"


class LibraryEntryGone(ValueError):
    """The entry a plan chose vanished or changed between the plan and the read (the caller re-plans)."""


# ---------------------------------------------------------------------------
# Value types
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class EncoderProvenance:
    """Which text encoder produced an embedding.

    `source`: the encoder variant id the client names (`stock`, `uncensored`,
    `""` = unknown); `format`: its weight-format id (`bf16`, `q8_0`, ...);
    `fp8`: weight-only fp8 was applied; `encoder_id`: the model package's
    fingerprint of the encoder files; `label`: a human-readable name (folder or
    file name) shown when the source is unknown.
    """

    source: str
    format: str
    fp8: bool
    encoder_id: str
    label: str

    def precision_id(self) -> str:
        """`"fp8"` when encoded with weight-only fp8, else the format id (the id the precision order ranks)."""
        return FP8_PRECISION if self.fp8 else self.format


@dataclass(frozen=True)
class LibraryEntry:
    """One valid `.msprompt` file of a library, as its header describes it.

    `file` is the bare file name inside the library directory; `shape` the
    tensor shape; `size_bytes` / `mtime_ns` the file's stat when the header was
    read (the reconcile key); `name` the preset name stored in the file.
    """

    file: str
    prompt: str
    name: str
    max_sequence_length: int
    dtype: str
    shape: tuple[int, ...]
    encoder: EncoderProvenance
    created_at: str
    size_bytes: int
    mtime_ns: int
    origin: str

    @property
    def width(self) -> int | None:
        """The embedding width (last tensor dimension), `None` for a scalar shape."""
        return self.shape[-1] if self.shape else None

    def same_identity(self, other: LibraryEntry) -> bool:
        """Whether both entries describe the same embedding: text, length, encoder and fp8."""
        return (self.prompt, self.max_sequence_length, self.encoder.encoder_id, self.encoder.fp8) == (other.prompt, other.max_sequence_length, other.encoder.encoder_id, other.encoder.fp8)


@dataclass(frozen=True)
class LibrarySpec:
    """What one model's library stores: its id (= directory name), the tensor name, the embedding dtype and its safetensors token."""

    library_id: str
    tensor_name: str
    dtype_name: str
    safetensors_dtype: str

    def __post_init__(self) -> None:
        require_library_id(self.library_id)


@dataclass(frozen=True)
class Preference:
    """The request's ranking preference: the selected encoder `source` (`""` = none) and the precision ids, most precise first."""

    source: str
    precision_order: tuple[str, ...]


@dataclass(frozen=True)
class LibrarySnapshot:
    """An immutable view of one library at its last reconcile.

    `entries` by file name; `skipped` maps a file name to why it is not an
    entry; `by_text` maps `(prompt, max_sequence_length)` to its entries,
    newest first.
    """

    library_id: str
    directory: Path
    entries: Mapping[str, LibraryEntry]
    skipped: Mapping[str, str]
    by_text: Mapping[tuple[str, int], tuple[LibraryEntry, ...]]
    #: The directory's mtime at the reconcile (`None`: it did not exist).
    directory_mtime_ns: int | None = field(default=None, compare=False)

    def entries_of_text(self, prompt: str) -> tuple[LibraryEntry, ...]:
        """Every entry of `prompt` (any sequence length), newest first."""
        found = [entry for entry in self.entries.values() if entry.prompt == prompt]
        return tuple(sorted(found, key=_newest_first_key))

    def texts(self) -> list[str]:
        """The distinct prompt texts, the one with the newest entry first (ties by text)."""
        newest: dict[str, int] = {}
        for entry in self.entries.values():
            newest[entry.prompt] = max(newest.get(entry.prompt, -1), created_epoch(entry.created_at))
        return sorted(newest, key=lambda text: (-newest[text], text))

    def preset_name(self, prompt: str) -> str:
        """The preset's display name: the first non-empty name among its entries, newest first; `""` when none has one."""
        for entry in self.entries_of_text(prompt):
            if entry.name:
                return entry.name
        return ""


# ---------------------------------------------------------------------------
# Small pure helpers
# ---------------------------------------------------------------------------
def require_library_id(library_id: str) -> str:
    """`library_id` when it matches `LIBRARY_ID_PATTERN`, else a `ValueError` naming it."""
    if not isinstance(library_id, str) or LIBRARY_ID_PATTERN.fullmatch(library_id) is None:
        raise ValueError(f"Недопустимый идентификатор библиотеки промптов: {library_id!r} (1–40 символов a-z, 0-9, «_»).")
    return library_id


def utc_now() -> str:
    """The current time in `CREATED_AT_FORMAT`."""
    return time.strftime(CREATED_AT_FORMAT, time.gmtime())


def created_epoch(created_at: str) -> int:
    """Seconds since the epoch of a `CREATED_AT_FORMAT` string; `0` (the oldest) when it does not parse."""
    try:
        return int(calendar.timegm(time.strptime(str(created_at), CREATED_AT_FORMAT)))
    except (TypeError, ValueError):
        return 0


def _newest_first_key(entry: LibraryEntry) -> tuple[int, str]:
    return (-created_epoch(entry.created_at), entry.file)


def is_name_removed_char(char: str) -> bool:
    """Whether `char` (one code point) is replaced by a space in a stored name (`NAME_REMOVED_RANGES`, or a lone surrogate)."""
    code = ord(char)
    if 0xD800 <= code <= 0xDFFF:
        # Not a Unicode scalar value, so the Rust client can never hold one
        # (parity is vacuous); JSON can still deliver a lone one here, and it
        # could not be UTF-8 encoded into a header.
        return True
    return any(low <= code <= high for low, high in NAME_REMOVED_RANGES)


def _clean_text(value: str, limit: int) -> str:
    """`value` with every `is_name_removed_char` code point replaced by a space, stripped, at most `limit` characters.

    `str.strip` and Rust's `str::trim` disagree only on U+001C..U+001F, which
    the table replaces first, so both sides trim the same characters.
    """
    cleaned = "".join(" " if is_name_removed_char(char) else char for char in str(value or ""))
    return cleaned.strip()[:limit].strip()


def normalize_preset_name(name: str) -> str:
    """A preset name as stored: every `NAME_REMOVED_RANGES` code point becomes a space, stripped, at most `MAX_PRESET_NAME_CHARS` code points.

    The Rust client applies the same rule (`normalize_preset_name` in
    `shared/prompt_library.rs`), so a typed name and the stored one compare
    equal; `fixtures/preset_name_normalization.json` pins both. A name is
    metadata only (never a path), so nothing else is restricted.
    """
    return _clean_text(name, MAX_PRESET_NAME_CHARS)


def default_preset_name(prompt: str) -> str:
    """The name a preset gets when the user gave none: the prompt's first `DEFAULT_NAME_CHARS` characters, whitespace collapsed (`…` when cut)."""
    flat = " ".join(str(prompt or "").split())
    if len(flat) <= DEFAULT_NAME_CHARS:
        return normalize_preset_name(flat)
    return normalize_preset_name(flat[:DEFAULT_NAME_CHARS].rstrip() + "…")


def _slug(text: str) -> str:
    """The prompt part of a file name: letters and digits kept, every other run of characters becomes one `_`, at most `SLUG_CHARS`."""
    out: list[str] = []
    for char in str(text):
        if char.isalnum():
            out.append(char)
        elif out and out[-1] != "_":
            out.append("_")
    return "".join(out).strip("_")[:SLUG_CHARS].strip("_")


def entry_file_name(prompt: str, max_sequence_length: int, encoder: EncoderProvenance) -> str:
    """The deterministic file name of one embedding: `<slug>-<text hash>-<encoder id prefix>[-fp8].msprompt`.

    One file per (text, length, encoder, fp8): a build, an import and a
    migration of the same embedding land on the same name, which makes all
    three idempotent. The slug is an allow-list (letters, digits, `_`), so no
    prompt can escape the directory; the text hash keeps prompts with the same
    slug apart. An empty prompt's slug is `empty`.
    """
    slug = _slug(prompt) or "empty"
    text_hash = sha256(f"{prompt}\0{int(max_sequence_length)}".encode("utf-8")).hexdigest()[:TEXT_HASH_CHARS]
    encoder_part = "".join(char for char in str(encoder.encoder_id).lower() if char in "0123456789abcdefghijklmnopqrstuvwxyz")[:ENCODER_ID_CHARS] or "unknown"
    suffix = "-fp8" if encoder.fp8 else ""
    return f"{slug}-{text_hash}-{encoder_part}{suffix}{ENTRY_SUFFIX}"


def require_entry_file_name(file: str) -> str:
    """`file` when it is a bare `.msprompt` file name (no directory part), else a `ValueError`.

    Client-supplied, so it is checked instead of being joined: a separator, a
    `..` or an absolute path would leave the library directory.
    """
    raw = str(file or "")
    if not raw or raw != Path(raw).name or raw in (".", "..") or "/" in raw or "\\" in raw or not raw.endswith(ENTRY_SUFFIX) or raw == ENTRY_SUFFIX:
        raise ValueError(f"Недопустимое имя файла кэша промпта: {raw!r}")
    return raw


# ---------------------------------------------------------------------------
# Selection: THE rule
# ---------------------------------------------------------------------------
def precision_rank(precision: str, order: Sequence[str]) -> int:
    """The index of `precision` in `order`; an id missing from it ranks after every listed one."""
    try:
        return list(order).index(precision)
    except ValueError:
        return len(order)


def rank_key(encoder: EncoderProvenance, created_at: str, in_memory: bool, file: str, pref: Preference) -> tuple[int, int, int, int, str]:
    """The ascending sort key of one candidate (smaller wins).

    1. the preferred encoder source first (only when the preference names one);
    2. the more precise precision id (`pref.precision_order`, unknown last);
    3. an embedding already in memory before a file (no read);
    4. the newer `created_at`;
    5. the file name (a stable tie-break).
    """
    preferred = 0 if pref.source and encoder.source == pref.source else 1
    return (preferred, precision_rank(encoder.precision_id(), pref.precision_order), 0 if in_memory else 1, -created_epoch(created_at), file)


def select_entry(
    candidates: Iterable[T],
    pref: Preference,
    *,
    provenance: Callable[[T], EncoderProvenance],
    created: Callable[[T], str],
    in_memory: Callable[[T], bool],
    name: Callable[[T], str],
) -> T | None:
    """THE selection rule: the candidate with the smallest `rank_key`, `None` when there is none. Pure.

    Callers filter first (exact text, sequence length, embedding width); this
    only ranks what is left.
    """
    return min(candidates, key=lambda item: rank_key(provenance(item), created(item), in_memory(item), name(item), pref), default=None)


def select_library_entry(entries: Iterable[LibraryEntry], pref: Preference) -> LibraryEntry | None:
    """`select_entry` over plain library entries (none of them in memory)."""
    return select_entry(entries, pref, provenance=lambda entry: entry.encoder, created=lambda entry: entry.created_at, in_memory=lambda _entry: False, name=lambda entry: entry.file)


# ---------------------------------------------------------------------------
# Container v2
# ---------------------------------------------------------------------------
def build_metadata(
    spec: LibrarySpec,
    *,
    prompt: str,
    name: str,
    max_sequence_length: int,
    encoder: EncoderProvenance,
    origin: str,
    created_at: str | None = None,
    extra: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """The `__metadata__` of a container v2 entry (strings only, as safetensors requires).

    `extra` holds informational fields (a model's encoder path, `migrated_from`);
    it may not override a reserved key.

    # Raises
    `ValueError` when an `extra` key is reserved, or a provenance / origin id
    breaks its pattern.
    """
    clash = sorted(set(extra or {}) & RESERVED_METADATA_KEYS)
    if clash:
        raise ValueError(f"Поля метаданных кэша промпта зарезервированы: {', '.join(clash)}")
    _require_provenance_ids(encoder)
    if ORIGIN_ID_PATTERN.fullmatch(origin) is None:
        raise ValueError(f"Недопустимое происхождение записи кэша промпта: {origin!r}")
    metadata = {str(key): str(value) for key, value in (extra or {}).items()}
    metadata.update(
        {
            "format": CONTAINER_FORMAT,
            "format_version": str(CONTAINER_VERSION),
            "library": spec.library_id,
            "name": normalize_preset_name(name),
            "prompt": str(prompt),
            "max_sequence_length": str(int(max_sequence_length)),
            "dtype": spec.dtype_name,
            "encoder_source": encoder.source,
            "encoder_format": encoder.format,
            "encoder_fp8": "true" if encoder.fp8 else "false",
            "encoder_id": str(encoder.encoder_id),
            "encoder_label": _clean_text(encoder.label, _MAX_LABEL_CHARS),
            "created_at": created_at or utc_now(),
            "origin": origin,
        }
    )
    return metadata


def _require_provenance_ids(encoder: EncoderProvenance) -> None:
    """Refuse a provenance whose source or format id breaks its pattern, or whose encoder id is empty."""
    if SOURCE_ID_PATTERN.fullmatch(encoder.source) is None:
        raise ValueError(f"Недопустимый источник энкодера: {encoder.source!r}")
    if FORMAT_ID_PATTERN.fullmatch(encoder.format) is None:
        raise ValueError(f"Недопустимый формат энкодера: {encoder.format!r}")
    if not str(encoder.encoder_id).strip():
        raise ValueError("У записи кэша промпта нет отпечатка энкодера.")


def parse_entry_header(header: Mapping[str, Any], spec: LibrarySpec, *, file: str, size_bytes: int, mtime_ns: int) -> LibraryEntry:
    """The `LibraryEntry` a container v2 header describes for `spec`'s library.

    Torch-free; the reasons are user-facing (a `skipped` row shows them).

    # Raises
    `ValueError` naming the first problem: not our format, another or newer
    version, another library, not exactly the one tensor of `spec`, another
    dtype (declared or the tensor's own token), a bad shape, length, id or flag.
    """
    raw_meta = header.get("__metadata__")
    if not isinstance(raw_meta, dict) or raw_meta.get("format") != CONTAINER_FORMAT:
        raise ValueError(f"не кэш промпта ManhwaStudio (нет метки «{CONTAINER_FORMAT}»)")
    meta = {str(key): str(value) for key, value in raw_meta.items()}
    version = _int_or_none(meta.get("format_version"))
    if version != CONTAINER_VERSION:
        if version is not None and version > CONTAINER_VERSION:
            raise ValueError(f"версия формата {version} новее поддерживаемой ({CONTAINER_VERSION}): обновите ManhwaStudio")
        raise ValueError(f"неподдерживаемая версия формата {meta.get('format_version')!r}")
    if meta.get("library") != spec.library_id:
        raise ValueError(f"запись другой библиотеки ({meta.get('library')!r}, ожидалась {spec.library_id!r})")
    tensors = {key: value for key, value in header.items() if key != "__metadata__"}
    tensor = tensors.get(spec.tensor_name)
    if set(tensors) != {spec.tensor_name} or not isinstance(tensor, dict):
        raise ValueError(f"файл должен содержать ровно один тензор «{spec.tensor_name}»")
    if tensor.get("dtype") != spec.safetensors_dtype or meta.get("dtype") != spec.dtype_name:
        raise ValueError(f"тип данных {meta.get('dtype')!r} / тензор {tensor.get('dtype')!r} вместо {spec.dtype_name!r} / {spec.safetensors_dtype!r}")
    shape = tensor.get("shape")
    if not isinstance(shape, list) or not shape or not all(isinstance(dim, int) and not isinstance(dim, bool) and dim >= 0 for dim in shape):
        raise ValueError(f"некорректная форма тензора {shape!r}")
    length = _int_or_none(meta.get("max_sequence_length"))
    if length is None or length <= 0:
        raise ValueError(f"некорректная длина последовательности {meta.get('max_sequence_length')!r}")
    fp8_text = meta.get("encoder_fp8")
    if fp8_text not in ("true", "false"):
        raise ValueError(f"некорректный флаг encoder_fp8 {fp8_text!r}")
    encoder = EncoderProvenance(
        source=meta.get("encoder_source", ""),
        format=meta.get("encoder_format", ""),
        fp8=fp8_text == "true",
        encoder_id=meta.get("encoder_id", ""),
        label=meta.get("encoder_label", ""),
    )
    _require_provenance_ids(encoder)
    origin = meta.get("origin", "")
    if ORIGIN_ID_PATTERN.fullmatch(origin) is None:
        raise ValueError(f"некорректное происхождение {origin!r}")
    return LibraryEntry(
        file=file,
        prompt=meta.get("prompt", ""),
        name=meta.get("name", ""),
        max_sequence_length=length,
        dtype=spec.dtype_name,
        shape=tuple(int(dim) for dim in shape),
        encoder=encoder,
        created_at=meta.get("created_at", ""),
        size_bytes=int(size_bytes),
        mtime_ns=int(mtime_ns),
        origin=origin,
    )


def _int_or_none(value: Any) -> int | None:
    try:
        return int(str(value))
    except (TypeError, ValueError):
        return None


def parse_payload_header(payload: bytes) -> tuple[dict[str, Any], int]:
    """`(header, data start)` of a safetensors container held in memory.

    # Raises
    `ValueError` when the bytes are not a safetensors container: too short, an
    implausible header length, a header that is not a UTF-8 JSON object, or a
    data section whose length is not the tensors' extent.
    """
    if len(payload) < 8:
        raise ValueError("файл слишком мал для safetensors")
    header_len = struct.unpack("<Q", payload[:8])[0]
    if header_len == 0 or header_len > _MAX_PAYLOAD_HEADER_BYTES or 8 + header_len > len(payload):
        raise ValueError(f"некорректная длина заголовка safetensors ({header_len})")
    try:
        header = json.loads(payload[8 : 8 + header_len].decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("заголовок safetensors не является JSON") from exc
    if not isinstance(header, dict):
        raise ValueError("заголовок safetensors не является объектом")
    data_start = 8 + header_len
    extent = 0
    for key, tensor in header.items():
        if key == "__metadata__":
            continue
        offsets = tensor.get("data_offsets") if isinstance(tensor, dict) else None
        if not isinstance(offsets, list) or len(offsets) != 2 or not all(isinstance(value, int) and not isinstance(value, bool) for value in offsets) or not 0 <= offsets[0] <= offsets[1]:
            raise ValueError(f"некорректные data_offsets тензора «{key}»")
        extent = max(extent, offsets[1])
    if extent != len(payload) - data_start:
        raise ValueError(f"секция данных {len(payload) - data_start} байт, а тензоры занимают {extent}")
    return header, data_start


def rewrite_safetensors_metadata(payload: bytes, metadata: Mapping[str, str]) -> bytes:
    """`payload` with its `__metadata__` replaced by `metadata`; the tensor bytes are copied verbatim.

    Header-only and torch-free. The new header is padded with spaces so the
    data section starts on an 8-byte boundary, as safetensors writers do.

    # Raises
    `ValueError` for a malformed container (`parse_payload_header`).
    """
    header, data_start = parse_payload_header(payload)
    header["__metadata__"] = {str(key): str(value) for key, value in metadata.items()}
    encoded = json.dumps(header, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    encoded += b" " * (-(8 + len(encoded)) % 8)
    return struct.pack("<Q", len(encoded)) + encoded + payload[data_start:]


def serialize_tensor(tensor_name: str, tensor: Any, metadata: Mapping[str, str]) -> bytes:
    """A safetensors container holding `tensor` as `tensor_name` with `metadata` (imports `safetensors.torch`). A patch point of the tests."""
    from safetensors.torch import save as safetensors_save

    return safetensors_save({tensor_name: tensor.contiguous()}, metadata=dict(metadata))


def load_tensor(path: Path, tensor_name: str) -> Any:
    """The `tensor_name` tensor of the container at `path` (imports `safetensors.torch`). A patch point of the tests.

    # Raises
    `ValueError` when the file does not carry that tensor.
    """
    from safetensors.torch import load_file

    tensor = load_file(str(path)).get(tensor_name)
    if tensor is None:
        raise ValueError(f"В файле кэша промпта {path} нет тензора «{tensor_name}».")
    return tensor


# ---------------------------------------------------------------------------
# Files
# ---------------------------------------------------------------------------
class UnsafeLibraryPath(ValueError):
    """A path the library will not read for a rewrite, replace or delete: a symbolic link / reparse point, the wrong kind, or outside its root."""


def is_link_like(info: os.stat_result) -> bool:
    """Whether an `lstat` result is a symbolic link or (Windows) any reparse point, junctions included."""
    return stat.S_ISLNK(info.st_mode) or bool(getattr(info, "st_reparse_tag", 0))


def require_real_directory(path: Path) -> Path:
    """`path` when it is an existing directory that is not itself a link (`lstat`), else `UnsafeLibraryPath`.

    Only `path` itself is checked: its ancestors are the user's own layout
    (the program root may legitimately live behind a link).
    """
    try:
        info = os.lstat(path)
    except OSError as exc:
        raise UnsafeLibraryPath(f"Каталог {path} недоступен: {exc}") from exc
    if is_link_like(info) or not stat.S_ISDIR(info.st_mode):
        raise UnsafeLibraryPath(f"{path} — не обычный каталог (символьная ссылка или файл): ManhwaStudio не пишет и не удаляет через ссылки.")
    return path


def require_confined(path: Path, root: Path, *, directory: bool, may_be_missing: bool = False) -> Path:
    """`path` when it is strictly inside the real directory `root` with no link on the way, else `UnsafeLibraryPath`.

    THE containment rule of every place the library (and the v1 migration)
    rewrites, replaces or deletes a file: `root` is a real directory
    (`require_real_directory`); `path` is lexically below it with no `..` /
    `.` component; every component from `root` down is `lstat`ed and is no
    symbolic link or reparse point, every intermediate one a directory and the
    last one a directory (`directory=True`) or a regular file; and the resolved
    path lies strictly inside the resolved `root`. With `may_be_missing` the
    last component may be absent (a file about to be created).

    The check guards against links that are there BEFORE the operation (a
    user's or a sync tool's), not against a concurrent attacker swapping a
    directory between the check and the call; callers re-check immediately
    before acting.
    """
    require_real_directory(root)
    try:
        parts = path.relative_to(root).parts
    except ValueError as exc:
        raise UnsafeLibraryPath(f"{path} лежит вне каталога {root}.") from exc
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise UnsafeLibraryPath(f"{path} не лежит строго внутри каталога {root}.")
    current = root
    for position, part in enumerate(parts):
        current = current / part
        last = position == len(parts) - 1
        try:
            info = os.lstat(current)
        except FileNotFoundError:
            if last and may_be_missing:
                break
            raise UnsafeLibraryPath(f"{current} не найден.") from None
        except OSError as exc:
            raise UnsafeLibraryPath(f"{current} недоступен: {exc}") from exc
        if is_link_like(info):
            raise UnsafeLibraryPath(f"{current} — символьная ссылка: ManhwaStudio не пишет и не удаляет через ссылки.")
        wants_directory = directory or not last
        if wants_directory and not stat.S_ISDIR(info.st_mode):
            raise UnsafeLibraryPath(f"{current} — не каталог.")
        if not wants_directory and not stat.S_ISREG(info.st_mode):
            raise UnsafeLibraryPath(f"{current} — не обычный файл.")
    try:
        resolved_root = root.resolve(strict=True)
        resolved = path.resolve(strict=False)
    except OSError as exc:
        raise UnsafeLibraryPath(f"Путь {path} не разрешается: {exc}") from exc
    if resolved == resolved_root or not resolved.is_relative_to(resolved_root):
        raise UnsafeLibraryPath(f"{path} не лежит строго внутри каталога {root}.")
    return path


def sync_directory(directory: Path) -> None:
    """Fsync `directory` so a rename, creation or removal inside it is on stable storage.

    The same policy as the Rust `ms_atomic_file::sync_directory`: on POSIX the
    directory is opened and fsynced, and a failure raises; elsewhere (Windows)
    it is a deliberate no-op, because a directory handle cannot be flushed
    there and the rename's durability rests on the filesystem's own metadata
    journal. A patch point of the order tests (always called through this
    module).

    # Raises
    `OSError` naming the directory when the open or the fsync fails (POSIX).
    """
    if os.name != "posix":
        return
    try:
        handle = os.open(directory, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    except OSError as exc:
        raise OSError(exc.errno, f"cannot open {directory} for fsync: {exc.strerror}") from exc
    try:
        os.fsync(handle)
    except OSError as exc:
        raise OSError(exc.errno, f"cannot fsync {directory}: {exc.strerror}") from exc
    finally:
        os.close(handle)


def remove_confined_file(path: Path, root: Path) -> None:
    """Delete the regular file `path` (strictly inside `root`, no link on the way) and fsync its directory.

    The containment is re-checked right before the unlink (`require_confined`),
    so a link is never followed; after the unlink the parent directory is
    fsynced (`sync_directory`) so the removal is durable.

    # Raises
    `UnsafeLibraryPath` when the path fails the containment rule (nothing is
    deleted); `OSError` from the unlink or the directory fsync.
    """
    require_confined(path, root, directory=False)
    os.unlink(path)
    sync_directory(path.parent)


def remove_confined_empty_directory(path: Path, root: Path) -> None:
    """Remove the empty real directory `path` (strictly inside `root`) and fsync its parent.

    # Raises
    `UnsafeLibraryPath` when the path fails the containment rule; `OSError`
    from the rmdir (for instance, not empty) or the fsync.
    """
    require_confined(path, root, directory=True)
    os.rmdir(path)
    sync_directory(path.parent)


def publish_bytes_atomically(dest: Path, payload: bytes) -> int:
    """Write `payload` to `dest` atomically and durably; returns the number of bytes written.

    The bytes go into a process-private `<name>.<pid>.part` sibling (created
    exclusively, so a link left at that name is never written through), are
    flushed and `fsync`ed, the handle is closed, a single `os.replace` makes
    `dest` appear, and the directory is fsynced (`sync_directory`) — so a crash
    or a full disk can never leave a truncated file where a valid one used to
    be, and once this returns the new entry survives a power loss. The staging
    file is removed on any failure before the replace. The pid keeps two
    backend processes apart. `dest.parent` must exist; the caller checks that
    `dest` itself is no link where that matters (`require_confined`).

    # Raises
    `OSError` from the write, the replace (the staging file is gone then) or
    the directory fsync (the file is published, its durability is not proven).
    """
    staging = dest.with_name(f"{dest.name}.{os.getpid()}.part")
    try:
        # A stale staging file of a crashed run with the same pid, or a link
        # someone left there: removing it unlinks the name only (never a target).
        os.unlink(staging)
    except FileNotFoundError:
        pass
    published = False
    try:
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
        with os.fdopen(os.open(staging, flags, 0o644), "wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(staging, dest)
        published = True
    finally:
        if not published:
            try:
                staging.unlink()
            except FileNotFoundError:
                pass
            except OSError as exc:
                log.warning("prompt library: could not remove the staging file %s: %s", staging, exc)
    sync_directory(dest.parent)
    return len(payload)


def require_entry_destination(path: str) -> Path:
    """Validate a client-supplied EXPORT path and return it.

    Absolute, the `.msprompt` suffix, a non-empty name, an existing parent
    directory (a save dialog always produces one), not an existing directory.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь для сохранения кэша промпта.")
    dest = Path(raw)
    if not dest.is_absolute():
        raise ValueError(f"Путь сохранения кэша промпта должен быть абсолютным: {raw}")
    if dest.suffix.lower() != ENTRY_SUFFIX:
        raise ValueError(f"Кэш промпта сохраняется только в файл «*{ENTRY_SUFFIX}», получено: {raw}")
    if not dest.name or dest.name == ENTRY_SUFFIX:
        raise ValueError(f"Пустое имя файла кэша промпта: {raw}")
    if dest.is_dir():
        raise ValueError(f"Путь сохранения кэша промпта — каталог: {raw}")
    if not dest.parent.is_dir():
        raise ValueError(f"Каталог для кэша промпта не найден: {dest.parent}")
    return dest


def require_entry_source(path: str) -> Path:
    """Validate a client-supplied IMPORT path and return it: absolute, the `.msprompt` suffix, an existing regular file.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь к файлу кэша промпта.")
    source = Path(raw)
    if not source.is_absolute():
        raise ValueError(f"Путь к кэшу промпта должен быть абсолютным: {raw}")
    if source.suffix.lower() != ENTRY_SUFFIX:
        raise ValueError(f"Кэш промпта читается только из файла «*{ENTRY_SUFFIX}», получено: {raw}")
    if not source.is_file():
        raise ValueError(f"Файл кэша промпта не найден: {raw}")
    return source


# ---------------------------------------------------------------------------
# The library
# ---------------------------------------------------------------------------
def _entry_to_row(entry: LibraryEntry) -> dict[str, Any]:
    """The `index.json` row of an entry (every field is also in the file's `__metadata__`)."""
    return {
        "size_bytes": entry.size_bytes,
        "mtime_ns": entry.mtime_ns,
        "prompt": entry.prompt,
        "prompt_sha256": sha256(entry.prompt.encode("utf-8")).hexdigest(),
        "name": entry.name,
        "max_sequence_length": entry.max_sequence_length,
        "dtype": entry.dtype,
        "shape": list(entry.shape),
        "encoder_source": entry.encoder.source,
        "encoder_format": entry.encoder.format,
        "encoder_fp8": entry.encoder.fp8,
        "encoder_id": entry.encoder.encoder_id,
        "encoder_label": entry.encoder.label,
        "created_at": entry.created_at,
        "origin": entry.origin,
    }


def _entry_from_row(file: str, row: Mapping[str, Any], spec: LibrarySpec) -> LibraryEntry:
    """The entry an index row describes.

    # Raises
    `ValueError` / `TypeError` / `KeyError` for a malformed row (the caller rebuilds the index).
    """
    shape = row["shape"]
    if not isinstance(shape, list) or not all(isinstance(dim, int) and not isinstance(dim, bool) for dim in shape):
        raise ValueError("bad shape")
    for key in ("size_bytes", "mtime_ns", "max_sequence_length"):
        if not isinstance(row[key], int) or isinstance(row[key], bool):
            raise ValueError(f"bad {key}")
    if not isinstance(row["encoder_fp8"], bool) or row["dtype"] != spec.dtype_name:
        raise ValueError("bad encoder_fp8 / dtype")
    texts = {key: row[key] for key in ("prompt", "name", "encoder_source", "encoder_format", "encoder_id", "encoder_label", "created_at", "origin")}
    if not all(isinstance(value, str) for value in texts.values()):
        raise ValueError("bad text field")
    entry = LibraryEntry(
        file=require_entry_file_name(file),
        prompt=texts["prompt"],
        name=texts["name"],
        max_sequence_length=row["max_sequence_length"],
        dtype=row["dtype"],
        shape=tuple(shape),
        encoder=EncoderProvenance(texts["encoder_source"], texts["encoder_format"], row["encoder_fp8"], texts["encoder_id"], texts["encoder_label"]),
        created_at=texts["created_at"],
        size_bytes=row["size_bytes"],
        mtime_ns=row["mtime_ns"],
        origin=texts["origin"],
    )
    _require_provenance_ids(entry.encoder)
    return entry


def _snapshot_of(spec: LibrarySpec, directory: Path, entries: Mapping[str, LibraryEntry], skipped: Mapping[str, tuple[int, int, str]], directory_mtime_ns: int | None) -> LibrarySnapshot:
    by_text: dict[tuple[str, int], list[LibraryEntry]] = {}
    for entry in entries.values():
        by_text.setdefault((entry.prompt, entry.max_sequence_length), []).append(entry)
    return LibrarySnapshot(
        library_id=spec.library_id,
        directory=directory,
        entries=MappingProxyType(dict(entries)),
        skipped=MappingProxyType({file: reason for file, (_size, _mtime, reason) in skipped.items()}),
        by_text=MappingProxyType({key: tuple(sorted(group, key=_newest_first_key)) for key, group in by_text.items()}),
        directory_mtime_ns=directory_mtime_ns,
    )


class PromptLibrary:
    """One model's library directory (`<root>/<library id>`): lazy load, reconcile, writes, validated reads.

    Thread-safety: every disk mutation and every reconcile runs under the
    instance lock (held for file-system work only, never across a tensor read
    or an encode); readers take the published `LibrarySnapshot`, which is
    immutable and replaced wholesale. Obtain instances through `library_for`.
    """

    def __init__(self, root: Path, spec: LibrarySpec) -> None:
        self._spec = spec
        self._root = Path(root)
        self._directory = self._root / spec.library_id
        self._lock = threading.Lock()
        self._snapshot: LibrarySnapshot | None = None
        # Reconcile state (only touched under `_lock`): valid entries and
        # skipped files with the stat they were judged at.
        self._entries: dict[str, LibraryEntry] = {}
        self._skipped: dict[str, tuple[int, int, str]] = {}
        self._index_loaded = False

    @property
    def directory(self) -> Path:
        """The library directory (not created until the first write)."""
        return self._directory

    @property
    def spec(self) -> LibrarySpec:
        return self._spec

    def path_of(self, file: str) -> Path:
        """The path of the entry file `file` (a bare file name, `require_entry_file_name`)."""
        return self._directory / require_entry_file_name(file)

    def confined_entry_path(self, file: str) -> Path:
        """`path_of(file)` after proving it is a regular file strictly inside the library root with no link on the way (`require_confined`).

        Every read whose bytes are copied or rewritten (export, rename) goes
        through this.

        # Raises
        `ValueError` for a malformed name; `UnsafeLibraryPath` when the file
        is missing, a link, not a regular file, or outside the root.
        """
        return require_confined(self.path_of(file), self._root, directory=False)

    def _ensure_directory_locked(self) -> None:
        """Create the root and the library directory when absent (each creation fsynced into its parent) and prove both real.

        # Raises
        `UnsafeLibraryPath` when the root or the library directory is a link
        or not a directory; `OSError` from a creation or a directory fsync.
        """
        if not os.path.lexists(self._root):
            self._root.mkdir(parents=True, exist_ok=True)
            sync_directory(self._root.parent)
        require_real_directory(self._root)
        if not os.path.lexists(self._directory):
            self._directory.mkdir(exist_ok=True)
            sync_directory(self._root)
        require_confined(self._directory, self._root, directory=True)

    def _publish_locked(self, path: Path, payload: bytes) -> None:
        """Publish `payload` at `path` (an entry or the index) inside the proven library directory; a link at `path` is refused, never replaced.

        # Raises
        `UnsafeLibraryPath` / `OSError` as `_ensure_directory_locked`,
        `require_confined` and `publish_bytes_atomically` raise them.
        """
        self._ensure_directory_locked()
        require_confined(path, self._root, directory=False, may_be_missing=True)
        publish_bytes_atomically(path, payload)

    def snapshot(self) -> LibrarySnapshot | None:
        """The last published snapshot; `None` before the first load. Lock-free."""
        return self._snapshot

    def entries_for(self, prompt: str, max_sequence_length: int) -> tuple[LibraryEntry, ...]:
        """The entries of exactly `prompt` at `max_sequence_length` in the published snapshot (newest first; empty when not loaded)."""
        snap = self._snapshot
        if snap is None:
            return ()
        return snap.by_text.get((prompt, int(max_sequence_length)), ())

    def load(self, *, blocking: bool) -> LibrarySnapshot | None:
        """The current snapshot, reconciling first when none exists yet or the directory changed since.

        "Changed" is the directory's mtime (an atomic replace, a new or removed
        file all change it); an in-place rewrite of a file that keeps the
        directory's mtime is caught by `reconcile` (`.list`) and by
        `read_tensor`'s own check. With `blocking=False` the published
        snapshot (possibly `None`) is returned when another thread holds the
        lock.
        """
        snap = self._snapshot
        if snap is not None and snap.directory_mtime_ns == self._directory_mtime_ns():
            return snap
        if not self._lock.acquire(blocking=blocking):
            return snap
        try:
            return self._reconcile_locked()
        finally:
            self._lock.release()

    def reconcile(self) -> LibrarySnapshot:
        """Bring the snapshot in line with the files (blocking) and return it."""
        with self._lock:
            return self._reconcile_locked()

    def _directory_mtime_ns(self) -> int | None:
        try:
            return self._directory.stat().st_mtime_ns
        except OSError:
            return None

    def _read_index_locked(self) -> None:
        """Seed the reconcile state from `index.json`; a missing, foreign, newer or malformed index leaves it empty (logged)."""
        self._index_loaded = True
        path = self._directory / INDEX_FILE
        try:
            raw = path.read_bytes()
        except FileNotFoundError:
            return
        except OSError as exc:
            log.warning("prompt library %s: index %s unreadable (%s); rebuilding from the files", self._spec.library_id, path, exc)
            return
        try:
            data = json.loads(raw.decode("utf-8"))
            if not isinstance(data, dict) or data.get("format") != INDEX_FORMAT or data.get("version") != INDEX_VERSION or data.get("library") != self._spec.library_id:
                raise ValueError(f"format {data.get('format') if isinstance(data, dict) else None!r}, version {data.get('version') if isinstance(data, dict) else None!r}")
            entries = {file: _entry_from_row(file, row, self._spec) for file, row in dict(data["entries"]).items()}
            skipped: dict[str, tuple[int, int, str]] = {}
            for file, row in dict(data["skipped"]).items():
                size, mtime, reason = row["size_bytes"], row["mtime_ns"], row["reason"]
                if not (isinstance(size, int) and isinstance(mtime, int) and isinstance(reason, str)):
                    raise ValueError("bad skipped row")
                skipped[require_entry_file_name(file)] = (size, mtime, reason)
        except (UnicodeDecodeError, json.JSONDecodeError, ValueError, TypeError, KeyError, AttributeError) as exc:
            log.warning("prompt library %s: index %s is not usable (%s); rebuilding from the files", self._spec.library_id, path, exc)
            return
        self._entries, self._skipped = entries, skipped

    def _write_index_locked(self) -> None:
        """Rewrite `index.json` atomically; a failure is logged (the index is a cache)."""
        data = {
            "format": INDEX_FORMAT,
            "version": INDEX_VERSION,
            "library": self._spec.library_id,
            "entries": {file: _entry_to_row(entry) for file, entry in sorted(self._entries.items())},
            "skipped": {file: {"size_bytes": size, "mtime_ns": mtime, "reason": reason} for file, (size, mtime, reason) in sorted(self._skipped.items())},
        }
        try:
            self._publish_locked(self._directory / INDEX_FILE, json.dumps(data, ensure_ascii=False, indent=1).encode("utf-8"))
        except (OSError, UnsafeLibraryPath) as exc:
            log.warning("prompt library %s: could not write the index (%s); it will be rebuilt", self._spec.library_id, exc)

    def _judge_locked(self, file: str, size: int, mtime_ns: int) -> LibraryEntry | str:
        """Read one file's header: its entry, or the reason it is skipped."""
        try:
            header = read_safetensors_header(self._directory / file)
            return parse_entry_header(header, self._spec, file=file, size_bytes=size, mtime_ns=mtime_ns)
        except ValueError as exc:
            log.warning("prompt library %s: %s skipped: %s", self._spec.library_id, file, exc)
            return str(exc)

    def _reconcile_locked(self) -> LibrarySnapshot:
        """Re-read every file whose (size, mtime) is new or changed, drop vanished ones, rewrite the index when anything changed, publish."""
        if not self._index_loaded:
            self._read_index_locked()
        directory_mtime = self._directory_mtime_ns()
        listing: dict[str, tuple[int, int]] = {}
        links: dict[str, tuple[int, int]] = {}
        listable = directory_mtime is not None
        if listable:
            try:
                require_confined(self._directory, self._root, directory=True)
            except UnsafeLibraryPath as exc:
                # A linked library (or root) directory is never listed: its
                # entries would be rewritten and replaced through the link.
                log.warning("prompt library %s: %s is not listed: %s", self._spec.library_id, self._directory, exc)
                listable = False
        if listable:
            try:
                with os.scandir(self._directory) as scan:
                    for item in scan:
                        if not item.name.endswith(ENTRY_SUFFIX) or item.name == ENTRY_SUFFIX:
                            continue
                        try:
                            info = item.stat(follow_symlinks=False)
                        except OSError as exc:
                            log.debug("prompt library %s: %s not statable (%s)", self._spec.library_id, item.name, exc)
                            continue
                        if is_link_like(info):
                            links[item.name] = (int(info.st_size), int(info.st_mtime_ns))
                        elif stat.S_ISREG(info.st_mode):
                            listing[item.name] = (int(info.st_size), int(info.st_mtime_ns))
            except OSError as exc:
                log.warning("prompt library %s: cannot list %s (%s)", self._spec.library_id, self._directory, exc)
        entries: dict[str, LibraryEntry] = {}
        skipped: dict[str, tuple[int, int, str]] = {}
        for file, (size, mtime) in sorted(links.items()):
            # Shown (so the user learns why the file is not used), never read or followed.
            if self._skipped.get(file, (None, None))[:2] != (size, mtime):
                log.warning("prompt library %s: %s is a symbolic link; not used", self._spec.library_id, file)
            skipped[file] = (size, mtime, LINK_SKIP_REASON)
        for file, (size, mtime) in sorted(listing.items()):
            known = self._entries.get(file)
            if known is not None and (known.size_bytes, known.mtime_ns) == (size, mtime):
                entries[file] = known
                continue
            judged_before = self._skipped.get(file)
            if judged_before is not None and judged_before[:2] == (size, mtime):
                skipped[file] = judged_before
                continue
            judged = self._judge_locked(file, size, mtime)
            if isinstance(judged, LibraryEntry):
                entries[file] = judged
            else:
                skipped[file] = (size, mtime, judged)
        changed = entries != self._entries or skipped != self._skipped
        self._entries, self._skipped = entries, skipped
        if changed and listable:
            self._write_index_locked()
            # The index write itself changes the directory's mtime.
            directory_mtime = self._directory_mtime_ns()
        snap = _snapshot_of(self._spec, self._directory, entries, skipped, directory_mtime)
        self._snapshot = snap
        return snap

    def _publish_entry_locked(self, file: str, payload: bytes) -> LibraryEntry:
        """Publish `payload` as `file`, re-read its header (the verification) and publish the new snapshot.

        # Raises
        `ValueError` naming the path when the write fails or the written file
        does not read back as an entry (the index is unchanged then).
        """
        path = self.path_of(file)
        try:
            self._publish_locked(path, payload)
            info = path.stat()
        except UnsafeLibraryPath as exc:
            raise UnsafeLibraryPath(f"Кэш промпта {path} не записан: {exc}") from exc
        except OSError as exc:
            raise ValueError(f"Не удалось записать кэш промпта {path}: {exc}") from exc
        judged = self._judge_locked(file, int(info.st_size), int(info.st_mtime_ns))
        if not isinstance(judged, LibraryEntry) or judged.size_bytes != len(payload):
            raise ValueError(f"Записанный кэш промпта {path} не читается обратно: {judged if isinstance(judged, str) else 'размер не совпал'}")
        self._entries = {**self._entries, file: judged}
        self._skipped = {key: value for key, value in self._skipped.items() if key != file}
        self._write_index_locked()
        self._snapshot = _snapshot_of(self._spec, self._directory, self._entries, self._skipped, self._directory_mtime_ns())
        return judged

    def _existing_locked(self, file: str, candidate: LibraryEntry) -> LibraryEntry | None:
        """The valid entry already at `file` with `candidate`'s identity; `None` when the name is free or holds a skipped file.

        # Raises
        `ValueError` when a VALID entry of another identity sits at that name
        (never overwritten: it is somebody's embedding).
        """
        snap = self._reconcile_locked()
        existing = snap.entries.get(file)
        if existing is None:
            if file in snap.skipped:
                log.warning("prompt library %s: replacing the unreadable %s (%s)", self._spec.library_id, file, snap.skipped[file])
            return None
        if not existing.same_identity(candidate):
            raise ValueError(f"Файл кэша промпта {self.path_of(file)} уже занят другой записью; переименуйте его.")
        return existing

    def write_entry(
        self,
        prompt: str,
        embeds: Any,
        encoder: EncoderProvenance,
        *,
        max_sequence_length: int,
        name: str,
        origin: str,
        extra: Mapping[str, str] | None = None,
    ) -> tuple[LibraryEntry, bool]:
        """Store one embedding; returns `(entry, newly written)`.

        An entry of the same identity already present is returned unchanged
        (`False`): file names are deterministic (`entry_file_name`). The
        tensor is serialized outside the lock.

        # Raises
        `ValueError` for invalid provenance, a tensor the library's spec does
        not accept (dtype, rank; nothing is written), a failed write (naming
        the path; the index is unchanged) or a foreign entry at the
        deterministic name.
        """
        metadata = build_metadata(self._spec, prompt=prompt, name=name, max_sequence_length=max_sequence_length, encoder=encoder, origin=origin, extra=extra)
        file = entry_file_name(prompt, max_sequence_length, encoder)
        probe = _probe_entry(file, prompt, max_sequence_length, encoder)
        with self._lock:
            existing = self._existing_locked(file, probe)
        if existing is not None:
            return existing, False
        payload = serialize_tensor(self._spec.tensor_name, embeds, metadata)
        # Checked before anything is published: a tensor of another dtype or
        # rank must never become a file the next reconcile skips.
        try:
            header, _start = parse_payload_header(payload)
            parse_entry_header(header, self._spec, file=file, size_bytes=len(payload), mtime_ns=0)
        except ValueError as exc:
            raise ValueError(f"Эмбеддинг не подходит библиотеке {self._spec.library_id}: {exc}") from exc
        with self._lock:
            existing = self._existing_locked(file, probe)
            if existing is not None:
                return existing, False
            entry = self._publish_entry_locked(file, payload)
        log.info("prompt library %s: wrote %s (%d bytes, origin %s)", self._spec.library_id, file, entry.size_bytes, origin)
        return entry, True

    def add_payload(self, payload: bytes, metadata: Mapping[str, str]) -> tuple[LibraryEntry, bool]:
        """Store an existing container (import, migration) under `metadata`, a complete `build_metadata` map of this library.

        The tensor bytes are kept verbatim; only the header is rewritten.
        Returns `(entry, newly written)` like `write_entry`.

        # Raises
        `ValueError` for a malformed container or metadata that does not
        describe a valid entry of this library, a failed write, or a foreign
        entry at the deterministic name.
        """
        rewritten = rewrite_safetensors_metadata(payload, metadata)
        header, _start = parse_payload_header(rewritten)
        provisional = parse_entry_header(header, self._spec, file=ENTRY_SUFFIX, size_bytes=len(rewritten), mtime_ns=0)
        file = entry_file_name(provisional.prompt, provisional.max_sequence_length, provisional.encoder)
        with self._lock:
            existing = self._existing_locked(file, provisional)
            if existing is not None:
                return existing, False
            entry = self._publish_entry_locked(file, rewritten)
        log.info("prompt library %s: added %s (%d bytes, origin %s)", self._spec.library_id, file, entry.size_bytes, entry.origin)
        return entry, True

    def rename_text(self, prompt: str, name: str) -> tuple[LibraryEntry, ...]:
        """Give every entry of `prompt` (any length) the preset name `name`; returns the entries that changed.

        A torch-free header rewrite of each file (tensor bytes verbatim, same
        file name, atomic).

        # Raises
        `ValueError` naming the file when one cannot be read or written (the
        entries rewritten before it keep the new name).
        """
        new_name = normalize_preset_name(name)
        changed: list[LibraryEntry] = []
        with self._lock:
            snap = self._reconcile_locked()
            for entry in snap.entries_of_text(prompt):
                if entry.name == new_name:
                    continue
                path = self.confined_entry_path(entry.file)
                try:
                    payload = path.read_bytes()
                except OSError as exc:
                    raise ValueError(f"Не удалось прочитать кэш промпта {path}: {exc}") from exc
                header, _start = parse_payload_header(payload)
                metadata = {str(key): str(value) for key, value in dict(header.get("__metadata__") or {}).items()}
                metadata["name"] = new_name
                changed.append(self._publish_entry_locked(entry.file, rewrite_safetensors_metadata(payload, metadata)))
        if changed:
            log.info("prompt library %s: renamed %d entries of one preset", self._spec.library_id, len(changed))
        return tuple(changed)

    def read_tensor(self, entry: LibraryEntry) -> Any:
        """The embedding of `entry`, after re-checking that the file still IS that entry.

        The file is re-statted; a changed (size, mtime) re-reads the header.
        A vanished, unreadable or re-purposed file reconciles the library
        (the row is dropped or marked skipped) and raises.

        # Raises
        `LibraryEntryGone` in those cases, or when the loaded tensor's shape is
        not the header's; whatever the tensor load raises otherwise.
        """
        path = self.path_of(entry.file)
        try:
            info = os.lstat(path)
        except OSError as exc:
            self.reconcile()
            raise LibraryEntryGone(f"Файл кэша промпта {path} пропал: {exc}") from exc
        if is_link_like(info) or not stat.S_ISREG(info.st_mode):
            self.reconcile()
            raise LibraryEntryGone(f"Файл кэша промпта {path} заменён ссылкой или каталогом.")
        if (int(info.st_size), int(info.st_mtime_ns)) != (entry.size_bytes, entry.mtime_ns):
            snap = self.reconcile()
            fresh = snap.entries.get(entry.file)
            if fresh is None or not fresh.same_identity(entry) or fresh.shape != entry.shape:
                raise LibraryEntryGone(f"Файл кэша промпта {path} изменился: {snap.skipped.get(entry.file, 'теперь это другая запись')}")
        try:
            tensor = load_tensor(path, self._spec.tensor_name)
        except FileNotFoundError as exc:
            self.reconcile()
            raise LibraryEntryGone(f"Файл кэша промпта {path} пропал: {exc}") from exc
        if tuple(int(dim) for dim in getattr(tensor, "shape", ())) != entry.shape:
            self.reconcile()
            raise LibraryEntryGone(f"Файл кэша промпта {path} изменился во время чтения.")
        return tensor


def _probe_entry(file: str, prompt: str, max_sequence_length: int, encoder: EncoderProvenance) -> LibraryEntry:
    """A stand-in entry carrying only the identity fields (`LibraryEntry.same_identity`)."""
    return LibraryEntry(file=file, prompt=prompt, name="", max_sequence_length=int(max_sequence_length), dtype="", shape=(), encoder=encoder, created_at="", size_bytes=0, mtime_ns=0, origin="")


_REGISTRY: dict[str, PromptLibrary] = {}
_REGISTRY_LOCK = threading.Lock()


def library_for(root: Path, spec: LibrarySpec) -> PromptLibrary:
    """The process-wide `PromptLibrary` of `root / spec.library_id` (one instance and one lock per directory).

    # Raises
    `ValueError` when the directory is already registered with another spec.
    """
    key = os.path.abspath(str(Path(root) / spec.library_id))
    with _REGISTRY_LOCK:
        library = _REGISTRY.get(key)
        if library is None:
            library = PromptLibrary(Path(root), spec)
            _REGISTRY[key] = library
        elif library.spec != spec:
            raise ValueError(f"Библиотека промптов {key} уже открыта с другим описанием ({library.spec} != {spec}).")
        return library
