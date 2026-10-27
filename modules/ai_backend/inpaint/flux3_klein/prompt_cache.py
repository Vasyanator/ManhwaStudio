"""
File: modules/ai_backend/inpaint/flux3_klein/prompt_cache.py

Purpose:
FLUX.3 klein's side of the shared prompt library (`runtime/prompt_library.py`,
orchestrated by `runtime/prompt_cache_service.py`): the library id, how the
Qwen3-VL text encoder that produced an embedding is identified, the embedding
width the DiT accepts, and the `PromptCacheAdapter` the service plugs in. Only
TEXT-mode encodes are cached (D11): in `vl_refs` mode the encoder sees the
references, and the region (reference #0) changes on every run, so such an
embedding would almost never be reused; the service never reads or writes the
library or the memory in that mode.

Encoding a caption costs reading the Qwen3-VL encoder (~8.9 GB bf16) while the
embedding is `[L, 20480]` bf16 (L = the padded token count: ~6.5 MB for a
typical 160-token caption, 168 MB at the 4096-token cap), so embeddings are
worth keeping, in memory and in the library.

Main responsibilities:
- `LIBRARY_ID`, `TENSOR`, `library_spec`, `prompt_cache_root`, `library` - the
  one FLUX.3 library (`prompt_cache/flux3_klein/`, every encoder format);
- `text_encoder_fingerprint`, `encoder_label`, `encoder_provenance` - the
  encoder identity (D11);
- `text_width` - the DiT's text width from the package `config.json`;
- `Flux3PromptCacheRequest` (the wire's prompt-library fields) and
  `Flux3CacheQuery` (what the adapter reads);
- `Flux3PromptCacheAdapter` - the `PromptCacheAdapter`.

Notes:
- Torch-free (the library loads tensors lazily); `wire.py` imports this module.
- `program_root` is the tests' patch point (through this module object).
- The source id is always `stock` (D12: FLUX.3 has one encoder model); the
  precision order is the client's (Rust `ms_config`: bf16 > q8_0 > q4_k_m).
- No legacy format exists: a container that is not v2 is refused on import.
"""

from __future__ import annotations

import hashlib
import json
import logging
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ...runtime import prompt_library as pl
from ...runtime.paths import program_root
from ...runtime.prompt_cache_service import refuse_unknown_marker
from .config import TEXT_MAX_TOKENS, EncoderSource, package_text_encoder_dir, text_encoder_availability, text_encoder_shards

log = logging.getLogger(__name__)

#: The FLUX.3 klein library id (a directory name and a persistence key: frozen;
#: the Rust client sends it as `prompt_library`).
LIBRARY_ID = "flux3_klein"

#: The single tensor an entry carries: `EncodedText.tokens`, `[L, 20480]` bf16,
#: `L` a multiple of 80 and at most 4096 (the DiT ids are derived, never stored).
TENSOR = "text_tokens"

#: The embedding dtype and its safetensors token (`run_text_encoder` returns bf16).
DTYPE_NAME = "bfloat16"
DTYPE_TOKEN = "BF16"

#: The encoder source id every FLUX.3 entry records (D12).
ENCODER_SOURCE = "stock"

#: The `max_sequence_length` of every FLUX.3 entry: the encoder's token cap
#: (the tensor's own length is the caption's padded length).
MAX_SEQUENCE_LENGTH = TEXT_MAX_TOKENS

#: Prompt embeddings kept in memory between runs, and their byte cap (E6): a
#: typical entry is ~6.5 MB, a 4096-token one 168 MB, so the count alone would
#: not bound the memory.
PROMPT_EMBED_CACHE_ENTRIES = 8
PROMPT_EMBED_CACHE_BYTES = 512 * 1024 * 1024

#: The encoder files of the identity besides the weights (`text_encoder/`).
_CONFIG_FILE = "config.json"
_CHAT_TEMPLATE_FILE = "chat_template.json"


@dataclass(frozen=True)
class Flux3PromptCacheRequest:
    """The prompt-library fields of a FLUX.3 request (`wire.py`).

    `library`: `LIBRARY_ID` or `None` (no library: memory only — the
    calibration tool, tests); `precision_order`: the client's precision ids,
    most precise first.
    """

    library: str | None = None
    precision_order: tuple[str, ...] = ()


@dataclass(frozen=True)
class Flux3CacheQuery:
    """What `Flux3PromptCacheAdapter` reads of one request.

    `prompt`: the exact (wire-stripped) caption; `model_root`: the package root
    (`None` for a lenient library call without one: no width, no current
    encoder); `encoder`: the selected encoder source; `width`: the DiT's text
    width (`None` = unknown, filters nothing).
    """

    prompt: str
    cache: Flux3PromptCacheRequest
    model_root: Path | None
    encoder: EncoderSource
    width: int | None


# =====================================================================
#  Encoder identity
# =====================================================================
def text_encoder_fingerprint(root: Path, source: EncoderSource) -> str:
    """Cheap, stable identity of the encoder `source` of the package under `root` (D11).

    `sha256(text_encoder/config.json bytes + "\\nchat_template\\0" + chat_template.json
    bytes (empty when absent))`, then for the bf16 shards `"\\n<name>\\0<size>"` per
    shard of the index, sorted, and for a GGUF file `"\\ngguf\\0<file name>\\0<size>"`.
    Reading gigabytes to identify a few megabytes would defeat the feature, so
    a fine-tune with an identical config and identical file sizes is not told
    apart.

    # Raises
    `FileNotFoundError` when the weights are absent (a missing shard, index or
    GGUF file); `ValueError` when `config.json` cannot be read or the shard
    index is broken (an unidentifiable encoder is an unloadable one).
    """
    directory = package_text_encoder_dir(root)
    config = directory / _CONFIG_FILE
    try:
        config_bytes = config.read_bytes()
    except OSError as exc:
        raise ValueError(f"FLUX.3 klein: не удалось прочитать {config}: {exc}. Без config.json энкодер нельзя ни опознать, ни загрузить.") from exc
    template = directory / _CHAT_TEMPLATE_FILE
    try:
        template_bytes = template.read_bytes()
    except FileNotFoundError:
        template_bytes = b""
    except OSError as exc:
        raise ValueError(f"FLUX.3 klein: не удалось прочитать {template}: {exc}") from exc
    digest = hashlib.sha256()
    digest.update(config_bytes)
    # The separator is hashed so a config/template split cannot collide with another.
    digest.update(b"\nchat_template\x00")
    digest.update(template_bytes)
    try:
        if source.path is None:
            weights = sorted((path.name, path.stat().st_size) for path in text_encoder_shards(directory))
            for name, size in weights:
                digest.update(f"\n{name}\x00{size}".encode("utf-8"))
        else:
            digest.update(f"\ngguf\x00{source.path.name}\x00{source.path.stat().st_size}".encode("utf-8"))
    except FileNotFoundError:
        raise
    except OSError as exc:
        raise ValueError(f"FLUX.3 klein: не удалось перечислить файлы текстового энкодера «{source.id}»: {exc}") from exc
    return digest.hexdigest()


def encoder_label(root: Path, source: EncoderSource) -> str:
    """A human-readable name of the encoder: the GGUF file's name, else `<package folder>/text_encoder`."""
    if source.path is not None:
        return source.path.name
    directory = package_text_encoder_dir(root)
    return f"{directory.parent.name}/{directory.name}" if directory.parent.name else directory.name


def encoder_provenance(root: Path, source: EncoderSource) -> pl.EncoderProvenance | None:
    """The provenance of the encoder `source` under `root`; `None` when its weights (or the `text_encoder/` directory) are absent.

    `source` is always `stock`, `format` the encoder format id, `fp8` false.

    # Raises
    `ValueError` when the encoder is present but cannot be identified
    (`text_encoder_fingerprint`).
    """
    reason, _size = text_encoder_availability(root, source)
    if reason is not None:
        return None
    try:
        encoder_id = text_encoder_fingerprint(root, source)
    except FileNotFoundError:
        # Deleted between the presence check and the fingerprint: absent.
        return None
    return pl.EncoderProvenance(source=ENCODER_SOURCE, format=source.id, fp8=False, encoder_id=encoder_id, label=encoder_label(root, source))


def encoder_available(root: Path | None, source: EncoderSource) -> bool:
    """Whether the encoder `source`'s weights and `text_encoder/` are present under `root` (`False` without a root)."""
    return root is not None and text_encoder_availability(root, source)[0] is None


def text_width(root: Path | None) -> int | None:
    """The DiT's text width (`dit.in_channels.text` of `<root>/config.json`); `None` when unknown.

    Lenient: a missing or malformed config filters nothing (the run's own
    checks diagnose it). The package's every DiT format shares the value.
    """
    if root is None:
        return None
    path = root / _CONFIG_FILE
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        value = data["dit"]["in_channels"]["text"]
    except (OSError, UnicodeDecodeError, json.JSONDecodeError, KeyError, TypeError) as exc:
        log.debug("FLUX.3 klein: ширина текста DiT не прочитана из %s (%s): записи библиотеки не фильтруются по ширине", path, exc)
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        log.debug("FLUX.3 klein: в %s ширина текста DiT %r не является положительным целым", path, value)
        return None
    return value


# =====================================================================
#  The library
# =====================================================================
def prompt_cache_root() -> Path:
    """The library root in the program root (`<root>/prompt_cache`); not created here (only a write creates it)."""
    return program_root() / pl.ROOT_DIRNAME


def library_spec(library_id: str = LIBRARY_ID) -> pl.LibrarySpec:
    """The `LibrarySpec` of a FLUX.3 library: one `text_tokens` bf16 tensor.

    # Raises
    `ValueError` for an id outside the library-id pattern.
    """
    return pl.LibrarySpec(pl.require_library_id(library_id), TENSOR, DTYPE_NAME, DTYPE_TOKEN)


def library(library_id: str = LIBRARY_ID) -> pl.PromptLibrary:
    """The process-wide library `prompt_cache/<library_id>` (`LIBRARY_ID` by default).

    # Raises
    `ValueError` for an invalid id.
    """
    return pl.library_for(prompt_cache_root(), library_spec(library_id))


# =====================================================================
#  The shared orchestration's adapter
# =====================================================================
class Flux3PromptCacheAdapter:
    """FLUX.3 klein's `PromptCacheAdapter` (`runtime.prompt_cache_service`); requests are `Flux3CacheQuery`s.

    One text per run (the caption: no negative prompt, the DiT is
    distilled); the length key is always `MAX_SEQUENCE_LENGTH`; no migration
    and no legacy format.
    """

    owner = "FLUX.3 klein"

    def library_of(self, library_id: str) -> pl.PromptLibrary:
        """The library `library_id` (`library`)."""
        return library(library_id)

    def accepts_library(self, library_id: str) -> bool:
        """Whether a v2 file of `library_id` may be imported: only `LIBRARY_ID`."""
        return library_id == LIBRARY_ID

    def library_id(self, request: Flux3CacheQuery) -> str | None:
        return request.cache.library or None

    def texts_needed(self, request: Flux3CacheQuery) -> list[str]:
        return [request.prompt]

    def max_sequence_length(self, request: Flux3CacheQuery) -> int:
        return MAX_SEQUENCE_LENGTH

    def preference(self, request: Flux3CacheQuery) -> pl.Preference:
        """Source `stock` (every entry's), then the client's precision order."""
        return pl.Preference(ENCODER_SOURCE, request.cache.precision_order)

    def accepted_width(self, request: Flux3CacheQuery) -> int | None:
        return request.width

    def current_provenance(self, request: Flux3CacheQuery) -> pl.EncoderProvenance | None:
        """`encoder_provenance` of the selected encoder (`None` without a root; raises `ValueError` when unidentifiable)."""
        if request.model_root is None:
            return None
        return encoder_provenance(request.model_root, request.encoder)

    def encoder_available(self, request: Flux3CacheQuery) -> bool:
        return encoder_available(request.model_root, request.encoder)

    def migrate(self, request: Flux3CacheQuery) -> None:
        """No legacy layout exists for FLUX.3: nothing to migrate."""

    def legacy_report(self) -> dict[str, Any] | None:
        return None

    def import_foreign(self, header: Mapping[str, Any], source: Path, request: Flux3CacheQuery) -> tuple[str, dict[str, str]]:
        """Every non-v2 container is refused (`refuse_unknown_marker`): FLUX.3 has no legacy format.

        # Raises
        `ValueError`, always.
        """
        return refuse_unknown_marker(source)
