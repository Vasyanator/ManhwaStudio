"""
File: modules/ai_backend/runtime/prompt_cache_service.py

Purpose:
The model-agnostic ORCHESTRATION of a model's prompt-embedding cache, on top of
the library (`prompt_library.py`): the in-memory embeddings a service keeps
between runs, THE answer to "which embedding serves each text a run needs and
which must be encoded", and the library surface the IPC methods expose
(`.status` cache keys, `.prompt_cache.build` / `.list` / `.export` /
`.import`). A model package plugs in through a `PromptCacheAdapter` (its
library ids, its texts, its encoder identity, its width check, its legacy
formats) and keeps everything model-specific: the encoder session, progress
frames, the answer's extra keys and its own refusals.

Main responsibilities:
- `MemoryEmbedding` / `memory_key` / `tensor_width` / `EmbeddingMemory` - the
  in-memory LRU (entry count, optional byte cap) and its copy-on-write
  provenance index;
- `PromptChoice` / `PromptPlan` / `PromptCacheOrchestrator.plan` - THE
  selection over memory and the library (`prompt_library.select_entry`);
- `entry_info` - the wire `EntryInfo` (protocol 23, `PROTOCOL.md §5.4`);
- `read_library_fields` - the three prompt-library wire fields;
- `PromptCacheOrchestrator` - `status`, `serve_locked`, `remember_locked`,
  `build_locked` (`BuildResult`), `listing`, `export`, `import_file`;
- `require_prompt_to_cache` / `refuse_unknown_marker` - the shared refusals.

Notes:
- Locking: the owning service's lock guards every MUTATION of the memory
  (`store`, `get`, and the orchestrator's `*_locked` methods); `plan`,
  `status`, `listing`, `export` and `import_file` take no service lock (the
  memory index is replaced wholesale on every change, so a lock-free reader
  iterates a consistent dict; the library has its own lock).
- Every log line and every model-naming refusal starts with the adapter's
  `owner` (e.g. `FLUX.2 klein`); the texts are those FLUX.2 always produced.
- Stdlib only; tensors are opaque (`shape`, and for the byte cap
  `numel()`/`element_size()` or `nbytes`).
"""

from __future__ import annotations

import logging
from collections import OrderedDict
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, Protocol

from . import prompt_library as pl

log = logging.getLogger(__name__)

#: The `origin` of an embedding encoded in this process and not (yet) in a library file.
MEMORY_ORIGIN = "memory"


# ---------------------------------------------------------------------------
# Value types
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class MemoryEmbedding:
    """Provenance of one in-memory embedding (the tensor itself lives in `EmbeddingMemory` under `key`).

    `library`: the library id of the request that produced it (`""` = none);
    `entry`: the library entry it was read from (`None` = encoded here);
    `width`: the tensor's last dimension.
    """

    key: tuple[Any, ...]
    library: str
    text: str
    max_sequence_length: int
    encoder: pl.EncoderProvenance
    width: int | None
    entry: pl.LibraryEntry | None
    created_at: str


@dataclass(frozen=True)
class PromptChoice:
    """The embedding that serves one text: from memory or from a file of the library `library` (`""` = none)."""

    text: str
    source: Literal["memory", "library"]
    entry: pl.LibraryEntry | MemoryEmbedding
    library: str

    @property
    def encoder(self) -> pl.EncoderProvenance:
        return self.entry.encoder


@dataclass(frozen=True)
class PromptPlan:
    """`PromptCacheOrchestrator.plan`'s answer: the choice per served text and the texts left to encode (in the order the run needs them)."""

    served: Mapping[str, PromptChoice]
    to_encode: tuple[str, ...]


@dataclass(frozen=True)
class BuildResult:
    """What `PromptCacheOrchestrator.build_locked` did.

    `prompt`: the preset's text; `name`: its (possibly new) preset name;
    `encoded`: some text had to be encoded (`encode_missing` was called);
    `written`: the entries newly written (an identical entry already present is
    not); `renamed`: a requested name was applied to the preset's entries;
    `current`: the encoder provenance every written entry carries.
    """

    prompt: str
    name: str
    encoded: bool
    written: tuple[pl.LibraryEntry, ...]
    renamed: bool
    current: pl.EncoderProvenance


def memory_key(library: str, text: str, max_sequence_length: int, encoder: pl.EncoderProvenance, origin_file: str) -> tuple[Any, ...]:
    """The in-memory cache key: library, text, length, encoder identity, precision, and the file it came from (`"encoded"` when encoded here)."""
    return (library, text, int(max_sequence_length), encoder.encoder_id, encoder.precision_id(), origin_file or "encoded")


def tensor_width(tensor: Any) -> int | None:
    """The last dimension of `tensor`'s `shape`; `None` for a shapeless or scalar value."""
    shape = tuple(getattr(tensor, "shape", ()) or ())
    return int(shape[-1]) if shape else None


def tensor_bytes(tensor: Any) -> int:
    """The bytes `tensor` occupies: `numel() * element_size()` (torch), else its `nbytes` (numpy), else 0 (an unknown object is not charged)."""
    numel = getattr(tensor, "numel", None)
    element_size = getattr(tensor, "element_size", None)
    if callable(numel) and callable(element_size):
        return int(numel()) * int(element_size())
    nbytes = getattr(tensor, "nbytes", None)
    if isinstance(nbytes, int):
        return nbytes
    return 0


def entry_info(entry: pl.LibraryEntry | MemoryEmbedding, current: pl.EncoderProvenance | None) -> dict[str, Any]:
    """The wire `EntryInfo` of a library entry or a memory embedding (protocol 23, PROTOCOL.md §5.4).

    A memory embedding read from a file reports that file's entry; one
    encoded here reports `origin: "memory"` with an empty `file` and `name`.
    `current_encoder`: made by the request's current encoder
    (`prompt_library.same_encoder`; false without one).
    """
    if isinstance(entry, MemoryEmbedding):
        if entry.entry is not None:
            return entry_info(entry.entry, current)
        return {
            "file": "",
            "name": "",
            "encoder_source": entry.encoder.source,
            "encoder_format": entry.encoder.format,
            "encoder_fp8": entry.encoder.fp8,
            "encoder_label": entry.encoder.label,
            "current_encoder": pl.same_encoder(entry.encoder, current),
            "created_at": entry.created_at,
            "size_bytes": 0,
            "origin": MEMORY_ORIGIN,
        }
    return {
        "file": entry.file,
        "name": entry.name,
        "encoder_source": entry.encoder.source,
        "encoder_format": entry.encoder.format,
        "encoder_fp8": entry.encoder.fp8,
        "encoder_label": entry.encoder.label,
        "current_encoder": pl.same_encoder(entry.encoder, current),
        "created_at": entry.created_at,
        "size_bytes": int(entry.size_bytes),
        "origin": entry.origin,
    }


# ---------------------------------------------------------------------------
# Wire fields and shared refusals
# ---------------------------------------------------------------------------
def read_library_fields(params: Mapping[str, Any] | None, *, owner: str) -> tuple[str | None, str, tuple[str, ...]]:
    """`(prompt_library, text_encoder_source, prompt_cache_precision_order)` of a raw params object (protocol 23).

    - `prompt_library`: the model's library id (`prompt_library.LIBRARY_ID_PATTERN`);
      absent or `null` = no library (memory only: the calibration tool, tests);
    - `text_encoder_source`: the selected encoder source id (1-32 of a-z, 0-9,
      `_`); absent, `null` or `""` = unknown (manual mode);
    - `prompt_cache_precision_order`: a list of distinct precision ids
      (`prompt_library.FORMAT_ID_PATTERN`), most precise first; absent or
      `null` = `()`.
    All three are opaque to the backend beyond their shape (Rust owns them).
    Reads no path, so it is valid before any component is configured.

    # Raises
    `ValueError` naming the field that breaks its contract, prefixed by `owner`.
    """
    merged = params or {}
    raw_library = merged.get("prompt_library")
    library: str | None = None
    if raw_library is not None:
        if not isinstance(raw_library, str) or pl.LIBRARY_ID_PATTERN.fullmatch(raw_library) is None:
            raise ValueError(f"{owner}: поле «prompt_library» должно быть идентификатором библиотеки (1–40 символов a-z, 0-9, «_»), получено {raw_library!r}")
        library = raw_library
    raw_source = merged.get("text_encoder_source")
    source = ""
    if raw_source is not None and raw_source != "":
        # Non-empty here, so the library's `{0,32}` source pattern means 1-32.
        if not isinstance(raw_source, str) or pl.SOURCE_ID_PATTERN.fullmatch(raw_source) is None:
            raise ValueError(f"{owner}: поле «text_encoder_source» должно быть идентификатором источника (1–32 символа a-z, 0-9, «_»), получено {raw_source!r}")
        source = raw_source
    raw_order = merged.get("prompt_cache_precision_order")
    order: tuple[str, ...] = ()
    if raw_order is not None:
        if not isinstance(raw_order, list):
            raise ValueError(f"{owner}: поле «prompt_cache_precision_order» должно быть списком идентификаторов, получено {raw_order!r}")
        order = tuple(_precision_id(item, f"prompt_cache_precision_order[{index}]", owner) for index, item in enumerate(raw_order))
        if len(set(order)) != len(order):
            raise ValueError(f"{owner}: в «prompt_cache_precision_order» есть повторы: {list(order)!r}")
    return library, source, order


def _precision_id(value: Any, field: str, owner: str) -> str:
    """`value` when it is a precision / weight-format id (`prompt_library.FORMAT_ID_PATTERN`), else a `ValueError` naming `field`."""
    if not isinstance(value, str) or pl.FORMAT_ID_PATTERN.fullmatch(value) is None:
        raise ValueError(
            f"{owner}: поле «{field}» должно быть идентификатором формата весов "
            f"(1–32 символа a-z, 0-9, «_»), получено {value!r}"
        )
    return value


def require_prompt_to_cache(text: str) -> None:
    """Refuse a build of an empty prompt (checked before anything else of a build).

    # Raises
    `ValueError` when `text` is empty.
    """
    if not text:
        raise ValueError("Кэшировать нечего: промпт пуст. Введите текст промпта и повторите.")


def refuse_unknown_marker(source: Path) -> tuple[str, dict[str, str]]:
    """The import refusal of a container whose format marker is neither v2 nor a legacy format the model knows (`PromptCacheAdapter.import_foreign`'s default).

    # Raises
    `ValueError`, always.
    """
    raise ValueError(f"Файл {source} не является кэшем промпта ManhwaStudio: нет известной метки формата.")


# ---------------------------------------------------------------------------
# Memory
# ---------------------------------------------------------------------------
class EmbeddingMemory:
    """The always-on in-memory embeddings of one service: an LRU of at most `max_entries` tensors and, when given, `max_bytes` bytes (`tensor_bytes`).

    Every mutation (`store`, `get`, which touches the LRU) happens under the
    owning service's lock. `index` (key -> `MemoryEmbedding`) is replaced
    wholesale on every change (copy-on-write), so a lock-free reader (the
    plan, `.status`, a forecast) iterates a consistent dict. The newest entry
    is never evicted by the byte cap, even when it alone exceeds it: the run
    that stored it is the reason it exists.
    """

    def __init__(self, max_entries: int, max_bytes: int | None = None) -> None:
        if max_entries < 1:
            raise ValueError(f"EmbeddingMemory: max_entries должен быть положительным, получено {max_entries}")
        if max_bytes is not None and max_bytes < 0:
            raise ValueError(f"EmbeddingMemory: max_bytes не может быть отрицательным, получено {max_bytes}")
        self._max_entries = int(max_entries)
        self._max_bytes = None if max_bytes is None else int(max_bytes)
        self._tensors: OrderedDict[tuple[Any, ...], Any] = OrderedDict()
        self._bytes: dict[tuple[Any, ...], int] = {}
        self._index: dict[tuple[Any, ...], MemoryEmbedding] = {}

    @property
    def index(self) -> Mapping[tuple[Any, ...], MemoryEmbedding]:
        """The provenance of every held tensor (copy-on-write; safe to iterate without the lock)."""
        return self._index

    @property
    def total_bytes(self) -> int:
        """The bytes the held tensors occupy (`tensor_bytes`)."""
        return sum(self._bytes.values())

    def store(self, remembered: MemoryEmbedding, tensor: Any) -> None:
        """Insert `tensor` under `remembered.key` as the newest entry, evicting the oldest ones over either cap; republishes `index`."""
        key = remembered.key
        self._tensors[key] = tensor
        self._tensors.move_to_end(key)
        self._bytes[key] = tensor_bytes(tensor)
        while len(self._tensors) > self._max_entries or (self._max_bytes is not None and len(self._tensors) > 1 and self.total_bytes > self._max_bytes):
            evicted, _tensor = self._tensors.popitem(last=False)
            self._bytes.pop(evicted, None)
        index = {**self._index, key: remembered}
        self._index = {item: value for item, value in index.items() if item in self._tensors}

    def get(self, key: tuple[Any, ...]) -> Any | None:
        """The tensor under `key` (now the newest entry), `None` when it was evicted."""
        tensor = self._tensors.get(key)
        if tensor is not None:
            self._tensors.move_to_end(key)
        return tensor

    def tensor_of(self, library: str, text: str, max_sequence_length: int, current: pl.EncoderProvenance) -> Any | None:
        """A held embedding of `text` in `library` at `max_sequence_length` made by the encoder `current`; `None` when there is none. Does not touch the LRU."""
        for key, remembered in self._index.items():
            if remembered.library == library and remembered.text == text and remembered.max_sequence_length == max_sequence_length and pl.same_encoder(remembered.encoder, current):
                tensor = self._tensors.get(key)
                if tensor is not None:
                    return tensor
        return None


# ---------------------------------------------------------------------------
# Adapter and orchestrator
# ---------------------------------------------------------------------------
class PromptCacheAdapter(Protocol):
    """What a model package tells the orchestrator. `request` is the model's own request object (opaque here).

    `owner` prefixes every log line and every model-naming refusal.
    `current_provenance` raises `ValueError` when an encoder is present but
    cannot be identified (the orchestrator turns that into "no current
    encoder" where browsing must not fail). `migrate` runs the model's
    one-time legacy migration for a request that names a library (a no-op
    when the model has none); `legacy_report` is the `.list` `legacy` object
    (`None` when nothing was found). `import_foreign` turns a container whose
    marker is not v2 into `(library id, v2 metadata)` or refuses it
    (`refuse_unknown_marker` when the model knows no legacy format).
    """

    owner: str

    def library_of(self, library_id: str) -> pl.PromptLibrary: ...

    def accepts_library(self, library_id: str) -> bool: ...

    def library_id(self, request: Any) -> str | None: ...

    def texts_needed(self, request: Any) -> list[str]: ...

    def max_sequence_length(self, request: Any) -> int: ...

    def preference(self, request: Any) -> pl.Preference: ...

    def accepted_width(self, request: Any) -> int | None: ...

    def current_provenance(self, request: Any) -> pl.EncoderProvenance | None: ...

    def encoder_available(self, request: Any) -> bool: ...

    def migrate(self, request: Any) -> None: ...

    def legacy_report(self) -> dict[str, Any] | None: ...

    def import_foreign(self, header: Mapping[str, Any], source: Path, request: Any) -> tuple[str, dict[str, str]]: ...


class PromptCacheOrchestrator:
    """The prompt-cache operations of one service over its `adapter` and its `memory` (see the module notes for the locking)."""

    def __init__(self, adapter: PromptCacheAdapter, memory: EmbeddingMemory) -> None:
        self._adapter = adapter
        self._memory = memory

    @property
    def memory(self) -> EmbeddingMemory:
        return self._memory

    # ---- the plan ----
    def plan(self, request: Any, *, blocking_load: bool) -> PromptPlan:
        """THE owner of "which embedding serves each text this run needs, and what must be encoded".

        Candidates of a text are the memory embeddings of the request's library
        (`""` = none) with the exact text and `max_sequence_length`, plus that
        library's entries with the same keys, minus every candidate whose
        width the model does not accept (`accepted_width`);
        `prompt_library.select_entry` picks one under the request's preference
        (selected encoder source, precision order). Lock-free: it reads the
        copy-on-write memory index and the library snapshot (`load`, blocking
        or not per `blocking_load`). Never migrates.
        """
        adapter = self._adapter
        library_id = adapter.library_id(request) or ""
        library = adapter.library_of(library_id) if library_id else None
        if library is not None:
            library.load(blocking=blocking_load)
        pref = adapter.preference(request)
        width = adapter.accepted_width(request)
        msl = adapter.max_sequence_length(request)
        remembered = self._memory.index
        served: dict[str, PromptChoice] = {}
        to_encode: list[str] = []
        for text in adapter.texts_needed(request):
            candidates: list[PromptChoice] = [
                PromptChoice(text, "memory", item, library_id)
                for item in remembered.values()
                if item.library == library_id and item.text == text and item.max_sequence_length == msl and (width is None or item.width is None or item.width == width)
            ]
            if library is not None:
                for entry in library.entries_for(text, msl):
                    if width is not None and entry.width != width:
                        log.debug("%s: запись %s шириной %s не подходит трансформеру (%d).", adapter.owner, entry.file, entry.width, width)
                        continue
                    candidates.append(PromptChoice(text, "library", entry, library_id))
            choice = pl.select_entry(
                candidates,
                pref,
                provenance=lambda candidate: candidate.encoder,
                created=lambda candidate: candidate.entry.created_at,
                in_memory=lambda candidate: candidate.source == "memory",
                name=lambda candidate: candidate.entry.file if isinstance(candidate.entry, pl.LibraryEntry) else (candidate.entry.entry.file if candidate.entry.entry is not None else ""),
            )
            if choice is None:
                to_encode.append(text)
            else:
                served[text] = choice
        return PromptPlan(served=served, to_encode=tuple(to_encode))

    def current_or_none(self, request: Any) -> pl.EncoderProvenance | None:
        """`adapter.current_provenance`, `None` (logged) when the encoder exists but cannot be identified: browsing never fails over it."""
        try:
            return self._adapter.current_provenance(request)
        except ValueError as exc:
            log.info("%s: текущий текстовый энкодер не опознан, записи библиотеки не сверяются с ним: %s", self._adapter.owner, exc)
            return None

    def status(self, request: Any) -> tuple[bool, dict[str, Any] | None]:
        """`(prompt_cached, prompt_cache_entry)` of `.status` for an already normalized `request`.

        `prompt_cached`: every text the run needs is served; the entry is the
        `EntryInfo` of the embedding that would serve the FIRST needed text
        (the prompt), `None` when none would. Lock-free, never migrates.
        """
        plan = self.plan(request, blocking_load=True)
        texts = self._adapter.texts_needed(request)
        choice = plan.served.get(texts[0]) if texts else None
        entry = None if choice is None else entry_info(choice.entry, self.current_or_none(request))
        return not plan.to_encode, entry

    # ---- serving and remembering (under the service lock) ----
    def serve_locked(self, choice: PromptChoice) -> Any | None:
        """The tensor of `choice` (caller holds the service lock); `None` when it is gone (a memory entry evicted, a library file vanished or changed).

        A library read is remembered in memory under its file's key, so the
        next run does not read the file again.
        """
        owner = self._adapter.owner
        if isinstance(choice.entry, MemoryEmbedding):
            return self._memory.get(choice.entry.key)
        entry = choice.entry
        library = self._adapter.library_of(choice.library)
        try:
            tensor = library.read_tensor(entry)
        except pl.LibraryEntryGone as exc:
            log.warning("%s: кэш промпта %s больше не годится (%s); запуск подберёт другой или закодирует промпт.", owner, entry.file, exc)
            return None
        remembered = MemoryEmbedding(
            key=memory_key(library.spec.library_id, entry.prompt, entry.max_sequence_length, entry.encoder, entry.file),
            library=library.spec.library_id,
            text=entry.prompt,
            max_sequence_length=entry.max_sequence_length,
            encoder=entry.encoder,
            width=tensor_width(tensor),
            entry=entry,
            created_at=entry.created_at,
        )
        self._memory.store(remembered, tensor)
        log.info("%s: промпт взят из библиотеки %s: %s (энкодер %s, %s).", owner, library.spec.library_id, entry.file, entry.encoder.label, entry.encoder.precision_id())
        return tensor

    def remember_locked(self, library: str, text: str, max_sequence_length: int, encoder: pl.EncoderProvenance, tensor: Any, created_at: str) -> None:
        """Keep an embedding ENCODED in this process (caller holds the service lock); `library` is the request's library id (`""` = none)."""
        remembered = MemoryEmbedding(
            key=memory_key(library, text, max_sequence_length, encoder, ""),
            library=library,
            text=text,
            max_sequence_length=max_sequence_length,
            encoder=encoder,
            width=tensor_width(tensor),
            entry=None,
            created_at=created_at,
        )
        self._memory.store(remembered, tensor)

    # ---- the library surface ----
    def require_library(self, request: Any) -> pl.PromptLibrary:
        """The library a library operation works on; the request must name one.

        # Raises
        `ValueError` when it names none (or an invalid id).
        """
        library_id = self._adapter.library_id(request)
        if not library_id:
            raise ValueError(f"{self._adapter.owner}: в запросе нет «prompt_library» — неизвестно, библиотеку какой модели открыть.")
        return self._adapter.library_of(library_id)

    def build_locked(
        self,
        request: Any,
        name: str | None,
        encode_missing: Callable[[list[str]], dict[str, Any]],
        *,
        current: pl.EncoderProvenance,
        extra: Mapping[str, str],
        on_nothing_to_encode: Callable[[bool], None],
    ) -> BuildResult:
        """«Кэшировать» under the service lock: save an entry of the encoder `current` for every text the run needs, under the preset name `name`.

        The caller has already refused an empty prompt
        (`require_prompt_to_cache`), resolved the library (`require_library`),
        run its migration and checked that its encoder is present (`current`
        is its provenance). A text that already has an entry of `current`
        (`prompt_library.same_encoder`) is not written again; the others reuse
        an embedding of `current` this process holds in memory, else
        `encode_missing(texts)` encodes them (the model's encoder session and
        progress) and answers `{text: tensor}`. When nothing must be encoded,
        `on_nothing_to_encode(any_to_write)` is called instead (the model's
        progress frame). `extra` is stored in every written entry's metadata.

        **The name.** A preset is the library's entries of one exact prompt
        text (the FIRST needed text); its name is stored in each entry. `name`
        empty or absent keeps the preset's current name (a new preset gets
        `default_preset_name`); a different name renames the preset — a header
        rewrite of all its entries, no encode. Other needed texts (a negative
        prompt) are written unnamed.

        # Raises
        Whatever `encode_missing` raises; `ValueError` for a failed write.
        """
        adapter = self._adapter
        library = self.require_library(request)
        texts = adapter.texts_needed(request)
        text = texts[0]
        msl = adapter.max_sequence_length(request)
        snap = library.reconcile()
        requested = pl.normalize_preset_name(name or "")
        preset_name = requested or snap.preset_name(text) or pl.default_preset_name(text)
        to_write = [needed for needed in texts if not any(pl.same_encoder(entry.encoder, current) for entry in library.entries_for(needed, msl))]
        tensors = {needed: tensor for needed in to_write if (tensor := self._memory.tensor_of(library.spec.library_id, needed, msl, current)) is not None}
        need = [needed for needed in to_write if needed not in tensors]
        if need:
            tensors.update(encode_missing(need))
        else:
            on_nothing_to_encode(bool(to_write))
        written: list[pl.LibraryEntry] = []
        for needed in to_write:
            entry, new = library.write_entry(needed, tensors[needed], current, max_sequence_length=msl, name=preset_name if needed == text else "", origin=pl.ORIGIN_BUILT, extra=dict(extra))
            if new:
                written.append(entry)
        renamed = bool(library.rename_text(text, preset_name)) if requested else False
        return BuildResult(prompt=text, name=preset_name, encoded=bool(need), written=tuple(written), renamed=renamed, current=current)

    def listing(self, request: Any) -> dict[str, Any]:
        """`.prompt_cache.list`: the presets of the request's library.

        Runs the model's one-time migration first and reconciles the library
        against its files. One row per prompt text (the empty negative prompt
        is never listed), newest first: `{prompt, name, selected, entries}`,
        where `entries` are the text's entries at the request's
        `max_sequence_length` whose width the model accepts, best first by the
        selection rule, and `selected` the first of them. A text without such
        entries is not listed.

        Returns `{"library", "directory", "texts", "skipped": [{file, reason}],
        "text_encoder_available", "legacy"}`.

        # Raises
        `ValueError` for a request without a library.
        """
        adapter = self._adapter
        library = self.require_library(request)
        adapter.migrate(request)
        snap = library.reconcile()
        current = self.current_or_none(request)
        pref = adapter.preference(request)
        width = adapter.accepted_width(request)
        msl = adapter.max_sequence_length(request)
        texts: list[dict[str, Any]] = []
        for text in snap.texts():
            if text == "":
                continue
            usable = [entry for entry in snap.by_text.get((text, msl), ()) if width is None or entry.width == width]
            if not usable:
                continue
            ranked = sorted(usable, key=lambda entry: pl.rank_key(entry.encoder, entry.created_at, False, entry.file, pref))
            texts.append({"prompt": text, "name": snap.preset_name(text), "selected": entry_info(ranked[0], current), "entries": [entry_info(entry, current) for entry in ranked]})
        return {
            "library": library.spec.library_id,
            "directory": str(library.directory),
            "texts": texts,
            "skipped": [{"file": file, "reason": reason} for file, reason in sorted(snap.skipped.items())],
            "text_encoder_available": adapter.encoder_available(request),
            "legacy": adapter.legacy_report(),
        }

    def export(self, request: Any, file: str, path: str) -> dict[str, Any]:
        """`.prompt_cache.export`: copy the entry `file` of the request's library to the `.msprompt` path `path` (a byte copy, atomic).

        An existing destination is overwritten: the path comes from the
        client's save dialog, where "replace?" was already asked.

        Returns `{"library", "file", "path", "size_bytes"}`.

        # Raises
        `ValueError` for a request without a library, a malformed file name,
        an entry that is not in the library, a source that is no longer a
        regular file inside the library (`prompt_library.UnsafeLibraryPath`),
        or a rejected destination (`require_entry_destination`).
        """
        library = self.require_library(request)
        if file not in library.reconcile().entries:
            raise ValueError(f"Кэша промпта «{file}» нет в библиотеке {library.directory}.")
        # The bytes copied out must be the library's own file, never a link's target.
        source = library.confined_entry_path(file)
        dest = pl.require_entry_destination(path)
        try:
            payload = source.read_bytes()
            size = pl.publish_bytes_atomically(dest, payload)
        except OSError as exc:
            raise ValueError(f"Не удалось скопировать кэш промпта {source} в {dest}: {exc}") from exc
        log.info("%s: кэш промпта %s выгружен в %s (%d байт).", self._adapter.owner, file, dest, size)
        return {"library": library.spec.library_id, "file": file, "path": str(dest), "size_bytes": int(size)}

    def import_file(self, request: Any, path: str) -> dict[str, Any]:
        """`.prompt_cache.import`: copy an outside `.msprompt` file into the library it belongs to.

        A v2 file goes into ITS OWN library (its `library` field; the model
        must accept it, `accepts_library`), whatever library the request
        names: `library_matches` says whether the two agree. Any other marker
        goes to `adapter.import_foreign` (a legacy conversion or a refusal).
        The tensor bytes are kept verbatim; an entry that is already present is
        not written again (`already_present`). The model's migration runs
        first.

        Returns `{"library", "file", "prompt", "name", "library_matches",
        "already_present", "origin"}`.

        # Raises
        `ValueError` for a rejected path, a file that is not a prompt cache, a
        library the model does not own, a refused legacy file, or a failed write.
        """
        adapter = self._adapter
        source = pl.require_entry_source(path)
        adapter.migrate(request)
        try:
            payload = source.read_bytes()
        except OSError as exc:
            raise ValueError(f"Не удалось прочитать файл кэша промпта {source}: {exc}") from exc
        try:
            header, _start = pl.parse_payload_header(payload)
        except ValueError as exc:
            raise ValueError(f"Файл {source} не является кэшем промпта ManhwaStudio: {exc}.") from exc
        raw_meta = header.get("__metadata__")
        marker = raw_meta.get("format") if isinstance(raw_meta, dict) else None
        if marker == pl.CONTAINER_FORMAT:
            library_id = str(raw_meta.get("library", ""))
            if not adapter.accepts_library(library_id):
                raise ValueError(f"Файл кэша промпта {source} относится к библиотеке «{library_id}», а не к {adapter.owner}.")
            metadata = {str(key): str(value) for key, value in raw_meta.items()}
            metadata["origin"] = pl.ORIGIN_IMPORTED
        else:
            library_id, metadata = adapter.import_foreign(header, source, request)
        library = adapter.library_of(library_id)
        entry, written = library.add_payload(payload, metadata)
        library_matches = library_id == adapter.library_id(request)
        log.info(
            "%s: кэш промпта %s импортирован в библиотеку %s как %s (%s).",
            adapter.owner,
            source,
            library_id,
            entry.file,
            "новая запись" if written else "уже была",
        )
        return {
            "library": library_id,
            "file": entry.file,
            "prompt": entry.prompt,
            "name": entry.name,
            "library_matches": library_matches,
            "already_present": not written,
            "origin": entry.origin,
        }
