"""
File: modules/ai_backend/inpaint/flux2_klein/prompt_cache.py

Purpose:
The prompt-cache library: the identity of the text encoder an embedding came
from, the `.msprompt` container that persists one embedding, and the on-disk
library those containers live in.

Encoding a prompt costs a 16 GB read of the Qwen3 encoder and the resulting
embedding is ~4 MiB, so embeddings are worth persisting. On-disk entries live in
`<program root>/prompt_cache/<encoder family>/<name>.msprompt` - a safetensors
container holding one `prompt_embeds` tensor plus a `__metadata__` map naming the
encoder. That name is CHECKED on load (`validate_prompt_file_metadata`):
embeddings of another encoder would load without an error and denoise into
something the user did not ask for.

Main responsibilities:
- encoder identity (`text_encoder_fingerprint`, `local_encoder_identity`,
  `encoder_family_name`);
- the container format (`prompt_file_metadata`, `read_prompt_file_header`,
  `validate_prompt_file_metadata`, `write_prompt_file`, `read_prompt_file_tensor`);
- library paths and listings (`prompt_cache_root`, `prompt_cache_family_dir`,
  `prompt_cache_entry_path`, `list_prompt_cache_entries`,
  `list_prompt_cache_families`, `find_prompt_cache_entry`);
- name sanitization and destination/source validation for import and export.

Notes:
- `program_root` and `write_prompt_file` are replaced by the test suite through
  this module object; consumers outside this file must reach `write_prompt_file`
  as `prompt_cache.write_prompt_file(...)` so that one patch reaches them all.
- torch and safetensors are imported lazily inside the functions that need them.
"""

from __future__ import annotations

import logging
import os
import time
from pathlib import Path
from typing import Any

from ...runtime.paths import program_root
from .components import (
    _MODEL_CONFIG_MARKER,
    component_dir_for_path,
    read_safetensors_header,
    text_encoder_available,
)
from .params import _to_bool, _to_int, text_encoder_dtype_name

log = logging.getLogger(__name__)

#: Prompt embeddings kept between runs. One entry is `[1, <=512, 4096]` at 2
#: bytes, i.e. ~4 MiB, so eight of them cost ~32 MiB — small enough to keep
#: always on, bounded so that a session of prompt edits is not a leak.
PROMPT_EMBED_CACHE_ENTRIES = 8

# ---------------------------------------------------------------------------
# The `.msprompt` prompt-cache file
# ---------------------------------------------------------------------------
# A prompt embedding is worth persisting for exactly one reason: producing it
# costs reading the Qwen3 encoder off disk - tens of seconds, and the bulk of
# what a fresh prompt costs - while the embedding itself is ~4 MiB. A user who always edits
# with the same prompt should never have to hold that encoder at all.
#
# The container is safetensors — already a dependency, readable by third-party
# tooling, and its header can be inspected without materializing a tensor, which
# is what makes the compatibility check below torch-free and cheap.

#: File suffix of a saved prompt cache. Checked on every client-supplied path:
#: those are untrusted input, and a wrong suffix is far more likely to be a
#: mis-wired path than a deliberate choice.
PROMPT_CACHE_SUFFIX = ".msprompt"

#: Library directory, in the program root next to `fonts/`. Layout:
#: `prompt_cache/<encoder family>/<entry name>.msprompt`. The family level
#: exists because an embedding is only valid for the encoder that produced it,
#: so entries of two encoders must never share a listing.
PROMPT_CACHE_DIRNAME = "prompt_cache"

#: Length of the fingerprint fragment appended to a family directory name. Eight
#: hex characters is 32 bits: enough that two encoders a user actually has
#: installed will not collide, short enough to keep the directory readable.
PROMPT_CACHE_FAMILY_HASH_CHARS = 8

#: Characters kept verbatim in a family or entry name. Everything else becomes
#: `_`. Path separators, `..`, control characters and the Windows-reserved
#: `<>:"/\|?*` are therefore all removed by construction rather than by a
#: blacklist that has to stay complete.
_SAFE_NAME_EXTRA = " ._-()"

#: Longest sanitized name component we write. Well under every filesystem's
#: limit even after the `.msprompt` suffix and a `.<pid>.part` staging suffix.
_MAX_NAME_LENGTH = 100

#: `__metadata__` marker. Present and equal, or the file is not ours and is
#: refused before anything else is looked at.
PROMPT_CACHE_FORMAT = "manhwastudio.flux2_klein.prompt_cache"

#: Format version. Bumped when the meaning of an existing field changes; a file
#: from a NEWER version is refused rather than read with today's rules.
PROMPT_CACHE_VERSION = 1

#: The single tensor a `.msprompt` file carries.
PROMPT_CACHE_TENSOR = "prompt_embeds"

#: safetensors dtype tokens for the two compute dtypes this service offers,
#: keyed by the `dtype` param name. The container records the tensor's own
#: dtype, so the declared one can be cross-checked against it from the header
#: alone — a hand-edited `__metadata__` cannot make float16 embeddings pass as
#: bfloat16 ones.
_PROMPT_CACHE_DTYPE_TOKENS = {"bfloat16": "BF16", "float16": "F16"}

#: Weight-file suffixes that take part in the text-encoder fingerprint.
_ENCODER_WEIGHT_SUFFIXES = (".safetensors", ".bin", ".pt", ".pth")


def local_encoder_identity(text_encoder_path: str) -> tuple[str, str] | None:
    """`(fingerprint, family)` of the encoder on disk, or `None` when there is none.

    The library needs an identity to file entries under and to compare a file
    against; without an encoder on this machine there is no identity to compute,
    and `None` is that fact rather than a placeholder. Callers must branch on it:
    `None` means "the file's own metadata is all we know", which is exactly what
    `validate_prompt_file_metadata` documents as the unverified path.

    # Raises
    `ValueError` when the path DOES exist but cannot be fingerprinted (no
    `config.json`, unreadable directory). An encoder that cannot be identified
    cannot be loaded either, so that stays an error instead of degrading into
    "no encoder" — degrading it would hide a broken checkout behind a silently
    weaker check.
    """
    if not text_encoder_available({"text_encoder_path": text_encoder_path}):
        return None
    encoder_id = text_encoder_fingerprint(text_encoder_path)
    return encoder_id, encoder_family_name(text_encoder_path, encoder_id)


# =====================================================================
#  The `.msprompt` prompt-cache file
# =====================================================================
def text_encoder_fingerprint(path: str) -> str:
    """Cheap, stable identity of the Qwen3 encoder a `.msprompt` file was built with.

    **Why an identity is needed at all.** Embeddings produced by a different
    encoder have the same shape and the same dtype as ours and load without a
    murmur — they simply denoise into something the user did not ask for. That
    is the silent wrong answer this package forbids everywhere else, so a saved
    file names the encoder it came from and `validate_prompt_file_metadata`
    refuses a mismatch.

    **Why not a hash of the weights.** The encoder is ~16 GB. Reading it to
    answer "may I load this 4 MiB file" would cost more than re-encoding the
    prompt, i.e. it would defeat the feature. The fingerprint is therefore taken
    from the METADATA of the directory: the bytes of `config.json` plus the
    sorted `(file name, size in bytes)` list of its weight files.

    What that catches: another model (a different architecture, hidden size,
    vocabulary or layer count AS DECLARED ON DISK — all of it lives in
    `config.json`; the truncation this service applies at LOAD time
    (`ENCODER_KEEP_LAYERS`) is invisible here, and deliberately so, since it
    provably does not change the hidden states the embedding is built from),
    another precision or another shard layout of the same model, a partially
    downloaded checkout, and a file swapped for one of a different length.

    What it does NOT catch: a fine-tune saved with the identical config and
    byte-for-byte identical file SIZES, and corruption inside a weight file that
    preserves its length. Both need the full read this function exists to avoid,
    and neither happens by accident — a user who deliberately replaces weights
    in place gets what they asked for.

    `path` may be the encoder directory or a weights file inside it
    (`component_dir_for_path`).

    # Raises
    `ValueError` when `path` does not resolve to a directory carrying a
    `config.json` — an encoder that cannot be identified is one that cannot be
    loaded either, so this is the same error the loader would raise later.
    """
    import hashlib

    source = component_dir_for_path(Path(path))
    if not source.is_dir():
        raise ValueError(f"Путь текстового энкодера должен быть каталогом: {source}")
    config = source / _MODEL_CONFIG_MARKER
    try:
        config_bytes = config.read_bytes()
    except OSError as exc:
        raise ValueError(
            f"Не удалось прочитать {config}: {exc}. Без config.json энкодер нельзя ни "
            "опознать, ни загрузить."
        ) from exc

    digest = hashlib.sha256()
    digest.update(config_bytes)
    try:
        weights = sorted(
            (entry.name, entry.stat().st_size)
            for entry in source.iterdir()
            if entry.is_file() and entry.suffix.lower() in _ENCODER_WEIGHT_SUFFIXES
        )
    except OSError as exc:
        raise ValueError(f"Не удалось перечислить файлы энкодера в {source}: {exc}") from exc
    for name, size in weights:
        # The separator is part of the hashed text so that ("ab", 1) and
        # ("a", 12) cannot collide into the same byte string.
        digest.update(f"\n{name}\x00{size}".encode("utf-8"))
    return digest.hexdigest()


def prompt_file_metadata(
    normalized: dict[str, Any], text: str, encoder_id: str, family: str
) -> dict[str, str]:
    """The `__metadata__` map of a `.msprompt` file. Values are strings; safetensors
    accepts nothing else.

    It carries everything `validate_prompt_file_metadata` needs to decide whether
    the embedding may be used, plus the ORIGINAL prompt text — which is the point
    of the file for the user: loading it must be able to show what was cached.

    `family` is the library subdirectory the entry belongs to. It is written into
    the file so that IMPORTING one on a machine (or at a moment) where another
    encoder is selected still files it under its own family instead of losing it
    among another encoder's entries.

    `dtype` is the ENCODER's (`text_encoder_dtype_name`), not the request's: the
    field describes the embedding stored in this file, and the request's `dtype`
    governs the transformer and the VAE, neither of which touched it.
    """
    return {
        "format": PROMPT_CACHE_FORMAT,
        "format_version": str(PROMPT_CACHE_VERSION),
        "prompt": str(text),
        "max_sequence_length": str(int(normalized["max_sequence_length"])),
        "dtype": text_encoder_dtype_name(),
        "text_encoder_fp8": "true" if normalized["text_encoder_fp8"] else "false",
        "text_encoder_id": str(encoder_id),
        "text_encoder_family": str(family),
        # Informational only. It is where the encoder lived when the file was
        # written, which helps a user recognize a file; it is NEVER what
        # compatibility is decided on, because a path proves nothing about the
        # weights that sit there now.
        "text_encoder_path": str(normalized["text_encoder_path"]),
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def read_prompt_file_header(path: Path) -> tuple[dict[str, str], dict[str, Any]]:
    """Read a `.msprompt` header: `(metadata, tensor description)`. No tensor is loaded.

    Deliberately torch-free and cheap — a file that is not ours, or that was
    built for another encoder, must be refused without allocating anything and
    without importing torch.

    # Raises
    `ValueError` when the file is not a safetensors container, carries no
    `__metadata__`, is not marked as our format, comes from a newer format
    version, or does not carry exactly the `prompt_embeds` tensor.
    """
    header = read_safetensors_header(path)
    raw_meta = header.get("__metadata__")
    if not isinstance(raw_meta, dict):
        raise ValueError(_foreign_prompt_file_message(path))
    metadata = {str(key): str(value) for key, value in raw_meta.items()}
    if metadata.get("format") != PROMPT_CACHE_FORMAT:
        raise ValueError(_foreign_prompt_file_message(path))
    version = _to_int(metadata.get("format_version"), 0)
    if version <= 0 or version > PROMPT_CACHE_VERSION:
        raise ValueError(
            f"Файл кэша промпта {path} версии «{metadata.get('format_version')}» не поддерживается "
            f"(эта сборка читает версии 1..{PROMPT_CACHE_VERSION}). Обновите ManhwaStudio или "
            "постройте кэш заново."
        )
    tensor = header.get(PROMPT_CACHE_TENSOR)
    if not isinstance(tensor, dict):
        raise ValueError(
            f"В файле кэша промпта {path} нет тензора «{PROMPT_CACHE_TENSOR}»: файл повреждён."
        )
    return metadata, tensor


def validate_prompt_file_metadata(
    metadata: dict[str, str],
    tensor: dict[str, Any],
    normalized: dict[str, Any],
    encoder_id: str | None,
    path: Path,
) -> None:
    """Refuse a `.msprompt` file that was not built for the current settings.

    Every field of `_prompt_cache_key` except the prompt text itself has to
    agree, because the text is what the file DEFINES while the rest is what the
    caller is about to run with. A disagreement is an explicit refusal naming the
    field: silently loading foreign embeddings would produce a plausible image
    that answers a different prompt, computed by a different encoder — a wrong
    result with no error attached to it.

    The encoder is compared by `text_encoder_fingerprint`, never by path; the
    tensor's own dtype is compared against the declared one, so an edited
    `__metadata__` cannot smuggle float16 embeddings into a bfloat16 run.

    **The dtype compared is the ENCODER's** (`text_encoder_dtype_name`), which is
    now always bfloat16, and NOT the request's `dtype` — that one governs the
    transformer and the VAE and cannot have changed a value in this tensor. The
    consequence is deliberate: a `.msprompt` written while the service still ran
    the encoder at float16 is refused from here on, because its embedding really
    did come from a different encoder precision. The refusal says so and says to
    rebuild the entry.

    **`encoder_id=None` means there is no encoder on this machine to compare
    against** (`local_encoder_identity`), which is the whole point of carrying a
    `.msprompt` to a machine that never downloaded the 16 GB Qwen3. Only the
    fingerprint comparison is skipped there — the format marker, the version, the
    sequence length, the dtype (metadata AND the tensor's own token) and the fp8
    flag are checked in every case, because none of them needs an encoder. The
    skip is not silent: it is logged here and reported to the client as
    `encoder_verified: false`, so "checked" and "taken on trust" are never
    presented as the same thing.

    # Raises
    `ValueError` naming the first field that does not match.
    """
    wanted_length = int(normalized["max_sequence_length"])
    file_length = _to_int(metadata.get("max_sequence_length"), 0)
    if file_length != wanted_length:
        raise ValueError(
            f"Файл кэша промпта {path} построен для max_sequence_length={file_length}, "
            f"а сейчас выбрано {wanted_length}. Эмбеддинги другой длины последовательности "
            "несовместимы — постройте кэш заново или верните прежнее значение."
        )

    wanted_dtype = text_encoder_dtype_name()
    file_dtype = metadata.get("dtype", "")
    if file_dtype != wanted_dtype:
        raise ValueError(
            f"Файл кэша промпта {path} построен для типа данных «{file_dtype}», а текстовый "
            f"энкодер FLUX.2 klein теперь всегда работает в «{wanted_dtype}»: он выполняется "
            "на CPU, где float16 приходится считать программно, и это в десятки раз медленнее. "
            "Эмбеддинги другой точности получены другим энкодером, поэтому запись нужно "
            "построить заново («Кэшировать») и сохранить под тем же именем."
        )
    expected_token = _PROMPT_CACHE_DTYPE_TOKENS.get(wanted_dtype)
    actual_token = str(tensor.get("dtype", ""))
    if expected_token is not None and actual_token != expected_token:
        raise ValueError(
            f"Файл кэша промпта {path} объявляет тип «{file_dtype}», но хранит тензор "
            f"{actual_token or '?'} вместо {expected_token}: файл повреждён или изменён вручную."
        )

    wanted_fp8 = bool(normalized["text_encoder_fp8"])
    file_fp8 = _to_bool(metadata.get("text_encoder_fp8"), False)
    if file_fp8 != wanted_fp8:
        raise ValueError(
            f"Файл кэша промпта {path} построен "
            f"{'с' if file_fp8 else 'без'} fp8-квантованием энкодера, а сейчас выбрано "
            f"{'с' if wanted_fp8 else 'без'} ним."
        )

    file_encoder = metadata.get("text_encoder_id", "")
    if encoder_id is None:
        # Nothing on this machine can produce a fingerprint, so the file's own
        # identity is all there is. It is still worth logging WHICH encoder the
        # embedding claims: that line is the only trace connecting a later
        # generation to the model it was really encoded with.
        log.info(
            "FLUX.2 klein: отпечаток энкодера для %s не сверялся — локального энкодера нет; "
            "файл объявляет энкодер %s (%s).",
            path,
            _short_id(file_encoder),
            metadata.get("text_encoder_family", "—"),
        )
        return
    if file_encoder != encoder_id:
        raise ValueError(
            f"Файл кэша промпта {path} построен другим текстовым энкодером "
            f"(в файле {_short_id(file_encoder)}, сейчас выбран {_short_id(encoder_id)}"
            f"{_encoder_origin_hint(metadata)}). Эмбеддинги чужого энкодера дали бы не ошибку, "
            "а неверный результат, поэтому файл отклонён."
        )


def _short_id(value: str) -> str:
    """First 12 hex characters of a fingerprint, for a user-facing message."""
    text = str(value or "")
    return text[:12] if text else "—"


def _encoder_origin_hint(metadata: dict[str, str]) -> str:
    """`, файл собран по пути …` when the file recorded one; empty otherwise."""
    origin = metadata.get("text_encoder_path", "")
    return f", файл собран по пути {origin}" if origin else ""


def _foreign_prompt_file_message(path: Path) -> str:
    """The message for a file that is not a `.msprompt` container at all."""
    return (
        f"Файл {path} не является кэшем промпта ManhwaStudio: в нём нет метки формата "
        f"«{PROMPT_CACHE_FORMAT}»."
    )


def require_prompt_file_destination(path: str) -> Path:
    """Validate a client-supplied SAVE path and return it.

    The path is untrusted input, so it is checked instead of being written to:
    it must be absolute (a relative one would land in the backend's working
    directory, which is not a place the user can find), it must carry the
    `.msprompt` suffix, its parent must already exist as a directory (a save
    dialog always produces one, so a missing parent means a mis-wired path
    rather than a folder to create), and it must not name an existing directory.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь для сохранения кэша промпта.")
    dest = Path(raw)
    if not dest.is_absolute():
        raise ValueError(f"Путь сохранения кэша промпта должен быть абсолютным: {raw}")
    if dest.suffix.lower() != PROMPT_CACHE_SUFFIX:
        raise ValueError(
            f"Кэш промпта сохраняется только в файл «*{PROMPT_CACHE_SUFFIX}», получено: {raw}"
        )
    if not dest.name or dest.name == PROMPT_CACHE_SUFFIX:
        raise ValueError(f"Пустое имя файла кэша промпта: {raw}")
    if dest.is_dir():
        raise ValueError(f"Путь сохранения кэша промпта — каталог: {raw}")
    if not dest.parent.is_dir():
        raise ValueError(f"Каталог для кэша промпта не найден: {dest.parent}")
    return dest


def require_prompt_file_source(path: str) -> Path:
    """Validate a client-supplied LOAD path and return it.

    Same suffix rule as the save side — the suffix is the format's name, and a
    file that does not carry it is not one we wrote — plus the file has to exist
    and be a regular file.

    # Raises
    `ValueError` naming what is wrong with the path.
    """
    raw = str(path or "").strip()
    if not raw:
        raise ValueError("Не задан путь к файлу кэша промпта.")
    source = Path(raw)
    if not source.is_absolute():
        raise ValueError(f"Путь к кэшу промпта должен быть абсолютным: {raw}")
    if source.suffix.lower() != PROMPT_CACHE_SUFFIX:
        raise ValueError(
            f"Кэш промпта читается только из файла «*{PROMPT_CACHE_SUFFIX}», получено: {raw}"
        )
    if not source.is_file():
        raise ValueError(f"Файл кэша промпта не найден: {raw}")
    return source


def publish_bytes_atomically(dest: Path, payload: bytes) -> int:
    """Write `payload` to `dest` atomically; returns the number of bytes written.

    The publish recipe is this project's (`engines/model_download.py`): the bytes
    go into a process-private `<name>.<pid>.part` sibling, are flushed and
    `fsync`ed, the handle is closed, and only then does a single `os.replace`
    make `dest` appear — so a crash or a full disk can never leave a truncated
    file where a valid cache used to be. The staging file is removed on any
    failure. The pid is in the staging name because a second backend process
    saving the same entry must not write into the same temporary file.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    staging = dest.with_name(f"{dest.name}.{os.getpid()}.part")
    published = False
    try:
        with staging.open("wb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(staging, dest)
        published = True
    finally:
        if not published:
            try:
                staging.unlink()
            except OSError:
                log.debug("FLUX.2 klein: не удалось удалить временный файл %s", staging)
    return len(payload)


def write_prompt_file(dest: Path, embeds: Any, metadata: dict[str, str]) -> int:
    """Serialize one embedding into a `.msprompt` file at `dest`; returns its size.

    `embeds` is the host-resident tensor from `_prompt_cache`. The bytes are
    built in memory (an entry is ~4 MiB) and published by
    `publish_bytes_atomically`.
    """
    from safetensors.torch import save as safetensors_save

    payload = safetensors_save({PROMPT_CACHE_TENSOR: embeds.contiguous()}, metadata=metadata)
    return publish_bytes_atomically(dest, payload)


def read_prompt_file_tensor(source: Path) -> Any:
    """Load the `prompt_embeds` tensor of an ALREADY VALIDATED `.msprompt` file.

    Callers must have run `read_prompt_file_header` +
    `validate_prompt_file_metadata` first: this function imports torch and
    allocates, and neither should happen for a file that is going to be refused.
    """
    from safetensors.torch import load_file

    tensors = load_file(str(source))
    embeds = tensors.get(PROMPT_CACHE_TENSOR)
    if embeds is None:
        raise ValueError(
            f"В файле кэша промпта {source} нет тензора «{PROMPT_CACHE_TENSOR}»: файл повреждён."
        )
    return embeds


# =====================================================================
#  The prompt-cache library: prompt_cache/<family>/<name>.msprompt
# =====================================================================
def prompt_cache_root() -> Path:
    """The library directory in the program root (`<root>/prompt_cache`).

    The root comes from `runtime.paths.program_root()`, which is this package's
    single owner of the directory-depth assumption; a local
    `Path(__file__).parents[N]` here would break the moment this module moves.
    The directory is NOT created by this function — only a write creates it.
    """
    return program_root() / PROMPT_CACHE_DIRNAME


def sanitize_name_component(name: str, *, what: str) -> str:
    """One filesystem-safe path COMPONENT, or a `ValueError` naming what was wrong.

    Both the family directory and the entry name are built from untrusted text
    (a user-chosen name, a directory name coming from a user-chosen model path),
    so the rule is an ALLOW-list rather than a blacklist of dangerous characters:
    letters, digits and `_SAFE_NAME_EXTRA` survive, everything else — path
    separators, control characters, the Windows-reserved set — becomes `_`. That
    makes `..`, `a/../../b` and an absolute path structurally impossible to
    express, instead of merely unlikely.

    Leading/trailing dots and spaces are stripped as well: a name like `..` or
    `.` would traverse, and a trailing dot or space is silently dropped by
    Windows, which would make the stored name differ from the reported one.

    Returns the sanitized component, truncated to `_MAX_NAME_LENGTH`.

    # Raises
    `ValueError` when nothing usable is left — an empty or dots-only name is a
    request error, never a silently invented placeholder.
    """
    raw = str(name or "")
    cleaned = "".join(
        char if (char.isalnum() or char in _SAFE_NAME_EXTRA) else "_" for char in raw
    )
    cleaned = cleaned.strip(" .")[:_MAX_NAME_LENGTH].strip(" .")
    if not cleaned or set(cleaned) <= {"_"}:
        raise ValueError(
            f"Недопустимое {what}: «{raw}». Оставьте буквы, цифры, пробел, «.», «_», «-»."
        )
    return cleaned


def encoder_family_name(text_encoder_path: str, encoder_id: str) -> str:
    """Library subdirectory for one text encoder: `<readable name>-<short id>`.

    Two halves, each doing a job the other cannot. The readable half is the
    encoder DIRECTORY's own name, so a user opening `prompt_cache/` sees which
    model a folder belongs to. The hash half is the first
    `PROMPT_CACHE_FAMILY_HASH_CHARS` of `text_encoder_fingerprint`, so two
    different encoders that happen to live in identically named directories
    (`.../model/text_encoder` is the common case) cannot pour their entries into
    one folder — where `load` would then offer embeddings the current encoder did
    not produce.

    The readable half is sanitized (`sanitize_name_component`), so a model
    directory named by the user cannot escape the library root. A directory whose
    name sanitizes to nothing falls back to `encoder`; the hash still keeps the
    family unique.
    """
    source = component_dir_for_path(Path(text_encoder_path))
    try:
        readable = sanitize_name_component(source.name, what="имя каталога энкодера")
    except ValueError:
        # A directory name made entirely of separators or exotic characters is
        # still a legitimate encoder; only its readability is lost.
        readable = "encoder"
    return f"{readable}-{str(encoder_id)[:PROMPT_CACHE_FAMILY_HASH_CHARS]}"


def prompt_cache_family_dir(family: str) -> Path:
    """Directory of one family inside the library. Not created by this call.

    `family` is sanitized again even when it came from a file we wrote: an
    imported `.msprompt` carries its family in metadata, and that metadata is
    attacker-controlled input like any other file content.
    """
    return prompt_cache_root() / sanitize_name_component(family, what="имя семейства энкодера")


def prompt_cache_entry_path(family: str, name: str) -> Path:
    """Full path of one library entry, with both components sanitized."""
    safe = sanitize_name_component(name, what="имя кэша промпта")
    return prompt_cache_family_dir(family) / f"{safe}{PROMPT_CACHE_SUFFIX}"


def list_prompt_cache_entries(family: str) -> tuple[list[dict[str, Any]], list[dict[str, str]]]:
    """List one family's entries: `(entries, skipped)`.

    A directory of caches is a place a user can drop files into, so a single
    corrupt or foreign file must not take the listing down with it: every file is
    read through `read_prompt_file_header` and, when that fails, it is reported in
    `skipped` with the reason instead of raising. Only the header is read, so a
    listing costs no tensor allocation and no torch import.

    Entries are sorted by name; the family directory not existing yet is an empty
    listing, not an error. Every entry names its own `family`, so a listing that
    spans several of them (the machine with no encoder — see
    `Flux2KleinInpaintService.prompt_cache_list`) stays unambiguous.
    """
    directory = prompt_cache_family_dir(family)
    entries: list[dict[str, Any]] = []
    skipped: list[dict[str, str]] = []
    if not directory.is_dir():
        return entries, skipped
    for path in sorted(directory.glob(f"*{PROMPT_CACHE_SUFFIX}")):
        if not path.is_file():
            continue
        try:
            metadata, _tensor = read_prompt_file_header(path)
        except ValueError as exc:
            skipped.append({"name": path.stem, "reason": str(exc)})
            continue
        try:
            size = int(path.stat().st_size)
        except OSError:
            size = 0
        entries.append(
            {
                "name": path.stem,
                # The directory the entry really sits in, not the argument: both
                # go through `sanitize_name_component`, and the stored one is what
                # a later `load` has to be given.
                "family": directory.name,
                "prompt": metadata.get("prompt", ""),
                "created_at": metadata.get("created_at", ""),
                "size_bytes": size,
                "max_sequence_length": _to_int(metadata.get("max_sequence_length"), 0),
                "dtype": metadata.get("dtype", ""),
            }
        )
    return entries, skipped


def list_prompt_cache_families() -> list[str]:
    """Family directory names present in the library, sorted; empty when there is none.

    Only the directory layout is read — no file is opened — because this exists
    to answer "which families could hold an entry" on a machine where no encoder
    is installed and the current family is therefore unknown.
    """
    root = prompt_cache_root()
    if not root.is_dir():
        return []
    try:
        return sorted(entry.name for entry in root.iterdir() if entry.is_dir())
    except OSError as exc:
        log.debug("FLUX.2 klein: не удалось перечислить библиотеку кэшей промптов (%s)", exc)
        return []


def find_prompt_cache_entry(name: str, family: str | None) -> Path:
    """Path of the library entry called `name`, looking it up across families when needed.

    With a `family` this is the plain `prompt_cache_entry_path` and the entry
    must be there — the current encoder's family is the only listing a
    generation may load from.

    `family is None` means no encoder is installed, so no family is the current
    one and the name is searched in ALL of them. An ambiguous name (the same
    entry name saved under two encoders) is REFUSED naming both families rather
    than resolved by an arbitrary rule: picking one would feed a generation
    embeddings from an encoder the user did not choose, which is the silent wrong
    answer this module refuses everywhere else.

    # Raises
    `ValueError` when nothing matches, or when several families do.
    """
    safe = sanitize_name_component(name, what="имя кэша промпта")
    if family is not None:
        path = prompt_cache_entry_path(family, safe)
        if not path.is_file():
            raise ValueError(
                f"Кэш промпта «{path.stem}» не найден в библиотеке семейства «{family}» "
                f"({path.parent})."
            )
        return path

    matches = [
        candidate
        for candidate in (
            prompt_cache_entry_path(known, safe) for known in list_prompt_cache_families()
        )
        if candidate.is_file()
    ]
    if not matches:
        raise ValueError(
            f"Кэш промпта «{safe}» не найден в библиотеке ({prompt_cache_root()}). "
            "Текстовый энкодер не выбран, поэтому поиск шёл по всем семействам."
        )
    if len(matches) > 1:
        families = ", ".join(f"«{match.parent.name}»" for match in matches)
        raise ValueError(
            f"Кэш промпта «{safe}» есть сразу в нескольких семействах ({families}), а текстовый "
            "энкодер не выбран — неизвестно, какой из них ваш. Выберите энкодер или "
            "переименуйте одну из записей."
        )
    return matches[0]


def require_free_entry_path(path: Path, *, overwrite: bool) -> Path:
    """Refuse an existing entry unless overwriting was asked for explicitly.

    Rebuilding a lost cache costs a 16 GB encoder read the user may have saved it
    precisely to avoid, so a name collision is a decision, not a detail: without
    `overwrite` it is a named error telling the caller both options. With
    `overwrite` the publish is still atomic, so a failed write cannot destroy the
    entry that is already there.

    # Raises
    `ValueError` when the entry exists and `overwrite` is false.
    """
    if path.exists() and not overwrite:
        raise ValueError(
            f"Кэш промпта «{path.stem}» уже существует в этой библиотеке. Выберите другое имя "
            "или разрешите перезапись (overwrite): восстановление удалённого кэша стоит "
            "полного чтения текстового энкодера."
        )
    return path
