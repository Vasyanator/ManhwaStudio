"""
File: modules/ai_backend/inpaint/flux2_klein/prompt_cache.py

Purpose:
FLUX.2 klein's side of the shared prompt library (`runtime/prompt_library.py`):
which library a request uses, how the text encoder that produced an embedding
is identified and described, the embedding width a transformer accepts, and the
one-time migration of the protocol-<=22 `.msprompt` files (container v1, one
directory per encoder "family") into the per-model libraries.

Encoding a prompt costs reading the Qwen3 encoder (up to 16 GB) while the
embedding is ~12.6 MB (9B) / ~7.9 MB (4B), so embeddings are worth keeping. Any
encoder variant of a model may serve that model's entries; the request's
preference ranks them (`runtime.prompt_library.select_entry`), and every entry
records its provenance so the UI can name it.

Main responsibilities:
- encoder identity: `text_encoder_fingerprint` (RoPE salt; `rope_salt=False`
  gives the pre-fix identity, used ONLY to recognize pre-fix v1 entries),
  `encoder_label`, `encoder_provenance` (the request's current encoder);
- libraries: `flux2_library_spec`, `prompt_cache_root`, `library_of`,
  `transformer_text_width`;
- v1: `read_v1_header`, `known_local_encoders`, `classify_v1_entry`,
  `convert_v1_metadata`, `migrate_legacy_families` / `ensure_legacy_migrated`.

Notes:
- `program_root` is the tests' patch point (through this module object).
- v1 migration rules (user decisions, protocol 23): an entry proven made before
  the RoPE fix (its id is the UNSALTED identity of an encoder whose RoPE the
  loader normalizes) and an unmatched entry of an uncensored encoder (post-fix
  unprovable) are DELETED; every other entry is written as a v2 file first,
  verified (header re-read, size) and only then is its v1 file deleted; an
  emptied family directory is removed; a file that cannot be migrated for any
  other reason stays where it is and is reported.
- Migration safety: the root, every family and every file is proven a real
  directory / regular file strictly inside the root before it is read and again
  before it is deleted (`prompt_library.require_confined`); a link is reported,
  never followed. A v1 file is unlinked only after its v2 copy's directory is
  fsynced (`prompt_library.sync_directory`), and each unlink is fsynced in its
  family directory.
- torch is never imported here.
"""

from __future__ import annotations

import logging
import os
import re
import stat
import threading
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from ...runtime import prompt_library as pl
from ...runtime.paths import program_root
from ...runtime.safetensors_stream import read_safetensors_header
from . import components
from .components import (
    _MODEL_CONFIG_MARKER,
    _config_int,
    _read_json_object,
    component_dir_for_path,
    is_gguf_file,
    text_encoder_available,
    text_encoder_config_dir,
    text_encoder_rope_kwargs,
    transformer_config_dir,
)
from .params import SAFETENSORS_FORMAT, GGUF_FORMAT, WEIGHT_FORMAT_ID_PATTERN, text_encoder_dtype_name

log = logging.getLogger(__name__)

#: The v2 library of each tensor width a v1 entry may carry (a Qwen3-4B / Qwen3-8B
#: encoder stacks three hidden states: 3 x 2560 / 3 x 4096). The ids are the Rust
#: `Flux2Variant::prompt_library_id` values (directory names: frozen); a Rust test
#: reads this table, so keep it a one-line literal with double-quoted ids.
LEGACY_LIBRARY_BY_WIDTH = {7680: "flux2_klein_4b", 12288: "flux2_klein_9b"}

#: Every FLUX.2 klein library id.
FLUX2_LIBRARY_IDS = frozenset(LEGACY_LIBRARY_BY_WIDTH.values())

#: Prompt embeddings kept in memory between runs. A 9B entry is `[1, 512, 12288]`
#: bf16, i.e. ~12.6 MB (4B: ~7.9 MB), so eight of them cost up to ~100 MB:
#: bounded, so that a session of prompt edits is not a leak.
PROMPT_EMBED_CACHE_ENTRIES = 8

#: The library root in the program root, next to `fonts/`. Layout:
#: `prompt_cache/<library id>/<entry>.msprompt` (+ `index.json`); protocol <= 22
#: left `prompt_cache/<encoder family>/<name>.msprompt` (v1).
PROMPT_CACHE_DIRNAME = "prompt_cache"

#: The single tensor an entry carries.
PROMPT_CACHE_TENSOR = "prompt_embeds"

#: safetensors dtype token of the encoder's dtype (`text_encoder_dtype_name`).
PROMPT_CACHE_DTYPE_TOKEN = "BF16"

#: `__metadata__["format"]` / version of a v1 entry (read by migration and import only).
V1_FORMAT = "manhwastudio.flux2_klein.prompt_cache"
V1_VERSION = 1

#: A v1 family directory name: `<readable>-<8 hex of the encoder fingerprint>`.
#: Library ids (`[a-z0-9_]`) never contain `-`, so the two never collide.
LEGACY_FAMILY_PATTERN = re.compile(r"^.+-[0-9a-f]{8}$")

#: The encoder subdirectories of an installed klein package and their source ids
#: (`ms_sysprobe` external catalog `flux2_klein.rs`: `text_encoder`,
#: `text_encoder_uncensored`, `text_encoder_gguf`, `text_encoder_uncensored_gguf`).
_PACKAGE_ENCODER_FOLDERS = (("text_encoder", "stock"), ("text_encoder_uncensored", "uncensored"))
_PACKAGE_ENCODER_GGUF_DIRS = (("text_encoder_gguf", "stock"), ("text_encoder_uncensored_gguf", "uncensored"))

#: Weight-file suffixes that take part in the text-encoder fingerprint.
_ENCODER_WEIGHT_SUFFIXES = (".safetensors", ".bin", ".pt", ".pth")


# =====================================================================
#  Encoder identity
# =====================================================================
def text_encoder_fingerprint(path: str, *, rope_salt: bool = True) -> str:
    """Cheap, stable identity of the Qwen3 encoder at `path` (a folder, a weights file inside it, or a GGUF file).

    **Why an identity.** Embeddings of a different encoder have the same shape
    and dtype and load without a murmur; the identity is what an entry's
    provenance and the in-memory key record, so "this encoder made it" is a
    fact, not a guess.

    **Why not a hash of the weights.** Reading 16 GB to identify a 12 MB file
    would defeat the feature. A folder's identity is `sha256(config.json bytes
    + "\\n<name>\\x00<size>" per weight file, sorted)`; a GGUF file's is
    `sha256(config bytes + "\\ngguf\\x00<file name>\\x00<size>")` over the
    config found for it (`components.text_encoder_config_dir`), so a quantized
    encoder never shares a folder's identity. It does NOT catch a fine-tune
    with identical config and file sizes, nor same-length corruption.

    **RoPE salt.** When the loader normalizes the config's RoPE base
    (`components.text_encoder_rope_kwargs` answers `{"rope_theta": v}`), the
    material gains `"\\nrope_theta=<repr(float(v))>"` last, so embeddings made
    while such an encoder ran with transformers' default base are never taken
    for current ones. `rope_salt=False` computes the PRE-FIX identity; only the
    v1 migration may ask for it.

    # Raises
    `ValueError` when `path` does not resolve to a directory carrying a
    `config.json` (or a GGUF file without one found for it), or when its RoPE
    is refused — an unidentifiable encoder is an unloadable one.
    """
    import hashlib

    gguf_file = Path(path)
    if is_gguf_file(gguf_file):
        return _gguf_encoder_fingerprint(gguf_file, rope_salt=rope_salt)
    source = component_dir_for_path(Path(path))
    if not source.is_dir():
        raise ValueError(f"Путь текстового энкодера должен быть каталогом: {source}")
    config = source / _MODEL_CONFIG_MARKER
    try:
        config_bytes = config.read_bytes()
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {config}: {exc}. Без config.json энкодер нельзя ни опознать, ни загрузить.") from exc
    digest = hashlib.sha256()
    digest.update(config_bytes)
    try:
        weights = sorted((entry.name, entry.stat().st_size) for entry in source.iterdir() if entry.is_file() and entry.suffix.lower() in _ENCODER_WEIGHT_SUFFIXES)
    except OSError as exc:
        raise ValueError(f"Не удалось перечислить файлы энкодера в {source}: {exc}") from exc
    for name, size in weights:
        # The separator is hashed so ("ab", 1) and ("a", 12) cannot collide.
        digest.update(f"\n{name}\x00{size}".encode("utf-8"))
    if rope_salt:
        _update_with_rope_salt(digest, source)
    return digest.hexdigest()


def _update_with_rope_salt(digest: Any, config_dir: Path) -> None:
    """Append the RoPE salt of `text_encoder_fingerprint` to `digest` when the loader normalizes `config_dir`'s RoPE (nothing otherwise).

    # Raises
    `ValueError` when the config's RoPE is refused (`text_encoder_rope_kwargs`).
    """
    rope = text_encoder_rope_kwargs(config_dir)
    if rope:
        digest.update(f"\nrope_theta={float(rope['rope_theta'])!r}".encode("utf-8"))


def _gguf_encoder_fingerprint(gguf_file: Path, *, rope_salt: bool) -> str:
    """`text_encoder_fingerprint` of a GGUF encoder file (see there).

    # Raises
    `ValueError` when no `config.json` is found for the file or a file cannot be read.
    """
    import hashlib

    try:
        config_dir = text_encoder_config_dir(str(gguf_file))
        config_bytes = (config_dir / _MODEL_CONFIG_MARKER).read_bytes()
        size = gguf_file.stat().st_size
    except OSError as exc:
        # `FileNotFoundError` (no config found) included: unidentifiable is unloadable.
        raise ValueError(f"Не удалось опознать GGUF-энкодер {gguf_file.name}: {exc}") from exc
    digest = hashlib.sha256()
    digest.update(config_bytes)
    digest.update(f"\ngguf\x00{gguf_file.name}\x00{size}".encode("utf-8"))
    if rope_salt:
        _update_with_rope_salt(digest, config_dir)
    return digest.hexdigest()


def encoder_label(path: str) -> str:
    """A human-readable name of the encoder at `path`: a GGUF file's name, else its folder's; a package's generic `text_encoder*` folder is prefixed with the package folder."""
    source = Path(str(path).rstrip("/\\") or str(path))
    if is_gguf_file(source):
        return source.name
    folder = component_dir_for_path(source)
    if folder.name.startswith("text_encoder") and folder.parent.name:
        return f"{folder.parent.name}/{folder.name}"
    return folder.name


def encoder_provenance(normalized: Mapping[str, Any]) -> pl.EncoderProvenance | None:
    """The provenance of the request's CURRENT encoder; `None` when no encoder is on this machine.

    `source` is the request's `text_encoder_source` (`""` in manual mode),
    `format` its `text_encoder_format` (derived from the container when absent).

    # Raises
    `ValueError` when the encoder exists but cannot be identified
    (`text_encoder_fingerprint`).
    """
    if not text_encoder_available(dict(normalized)):
        return None
    path = str(normalized["text_encoder_path"])
    fmt = str(normalized.get("text_encoder_format") or "")
    if WEIGHT_FORMAT_ID_PATTERN.fullmatch(fmt) is None:
        fmt = GGUF_FORMAT if is_gguf_file(Path(path)) else SAFETENSORS_FORMAT
    return pl.EncoderProvenance(
        source=str(normalized.get("text_encoder_source") or ""),
        format=fmt,
        fp8=bool(normalized.get("text_encoder_fp8")),
        encoder_id=text_encoder_fingerprint(path),
        label=encoder_label(path),
    )


def is_current_encoder(encoder: pl.EncoderProvenance, current: pl.EncoderProvenance | None) -> bool:
    """Whether `encoder` IS the request's current encoder: same fingerprint and the same fp8 flag."""
    return current is not None and encoder.encoder_id == current.encoder_id and encoder.fp8 == current.fp8


# =====================================================================
#  Libraries
# =====================================================================
def prompt_cache_root() -> Path:
    """The library root in the program root (`<root>/prompt_cache`); not created here (only a write creates it).

    The root comes from `runtime.paths.program_root()`, the package's single
    owner of the directory-depth assumption.
    """
    return program_root() / PROMPT_CACHE_DIRNAME


def flux2_library_spec(library_id: str) -> pl.LibrarySpec:
    """The `LibrarySpec` of a FLUX.2 library: one `prompt_embeds` tensor in the encoder's dtype.

    # Raises
    `ValueError` for an id outside the library-id pattern.
    """
    return pl.LibrarySpec(pl.require_library_id(library_id), PROMPT_CACHE_TENSOR, text_encoder_dtype_name(), PROMPT_CACHE_DTYPE_TOKEN)


def library_of(library_id: str) -> pl.PromptLibrary:
    """The process-wide library `prompt_cache/<library_id>`.

    # Raises
    `ValueError` for an invalid id.
    """
    return pl.library_for(prompt_cache_root(), flux2_library_spec(library_id))


def transformer_text_width(transformer_path: str) -> int | None:
    """The embedding width the transformer at `transformer_path` accepts (`joint_attention_dim` of its config); `None` when unknown.

    A library entry of another width cannot feed this transformer, so the
    selection drops it; an unreadable config filters nothing (the run's own
    guards diagnose it).
    """
    if not transformer_path:
        return None
    config_dir = transformer_config_dir(str(transformer_path))
    if config_dir is None:
        return None
    return _config_int(_read_json_object(config_dir / _MODEL_CONFIG_MARKER), "joint_attention_dim")


# =====================================================================
#  v1 (protocol <= 22) entries: classification, conversion, migration
# =====================================================================
def read_v1_header(path: Path) -> tuple[dict[str, str], dict[str, Any]]:
    """`(metadata, tensor description)` of a v1 `.msprompt` file. No tensor is loaded.

    # Raises
    `ValueError` when the file is not a safetensors container, is not a v1
    entry (marker, version) or does not carry the `prompt_embeds` tensor.
    """
    return parse_v1_header(read_safetensors_header(path), path)


def parse_v1_header(header: Mapping[str, Any], path: Path | str) -> tuple[dict[str, str], dict[str, Any]]:
    """`read_v1_header` over an already parsed header (`path` names the file in messages).

    # Raises
    As `read_v1_header`.
    """
    raw_meta = header.get("__metadata__")
    if not isinstance(raw_meta, dict) or raw_meta.get("format") != V1_FORMAT:
        raise ValueError(f"Файл {path} не является кэшем промпта ManhwaStudio версии 1.")
    metadata = {str(key): str(value) for key, value in raw_meta.items()}
    if metadata.get("format_version") != str(V1_VERSION):
        raise ValueError(f"Файл кэша промпта {path}: неподдерживаемая версия «{metadata.get('format_version')}».")
    tensor = header.get(PROMPT_CACHE_TENSOR)
    if not isinstance(tensor, dict):
        raise ValueError(f"В файле кэша промпта {path} нет тензора «{PROMPT_CACHE_TENSOR}»: файл повреждён.")
    return metadata, dict(tensor)


@dataclass(frozen=True)
class KnownEncoder:
    """One locally findable encoder identity (`known_local_encoders`).

    `salted`: this fingerprint is the encoder's CURRENT identity (False: its
    pre-RoPE-fix identity); `needs_rope`: the loader normalizes its RoPE (the
    two identities differ); `source`: `stock` / `uncensored` for a package
    encoder, the request's source for the current one, `""` otherwise.
    """

    salted: bool
    needs_rope: bool
    source: str
    format: str
    label: str


def _package_encoder_paths() -> list[tuple[str, str, str]]:
    """`(path, source, format)` of every encoder inside the installed klein packages (folders first, then GGUF files)."""
    found: list[tuple[str, str, str]] = []
    for root in components.installed_klein_package_roots():
        for subdir, source in _PACKAGE_ENCODER_FOLDERS:
            if (root / subdir).is_dir():
                found.append((str(root / subdir), source, SAFETENSORS_FORMAT))
        for subdir, source in _PACKAGE_ENCODER_GGUF_DIRS:
            folder = root / subdir
            try:
                files = sorted(path for path in folder.glob("*.gguf") if path.is_file()) if folder.is_dir() else []
            except OSError as exc:
                log.debug("FLUX.2 klein: GGUF-энкодеры в %s не перечислены: %s", folder, exc)
                files = []
            found.extend((str(path), source, GGUF_FORMAT) for path in files)
    return found


def known_local_encoders(extra: Iterable[tuple[str, str]]) -> dict[str, KnownEncoder]:
    """Fingerprint -> `KnownEncoder` of every encoder findable on this machine: the installed klein packages, then `extra` `(path, source)` pairs (the request's encoder, recorded v1 paths).

    Both identities are registered (current and, when they differ, the
    pre-RoPE-fix one); an earlier candidate wins, so a package attribution beats
    a recorded path. A path that is missing or cannot be identified is skipped
    (debug log).
    """
    known: dict[str, KnownEncoder] = {}
    candidates = _package_encoder_paths() + [(path, source, "") for path, source in extra if path]
    for path, source, fmt in candidates:
        if not Path(path).exists():
            continue
        try:
            salted = text_encoder_fingerprint(path)
            unsalted = text_encoder_fingerprint(path, rope_salt=False)
        except ValueError as exc:
            log.debug("FLUX.2 klein: энкодер %s не опознан при переносе кэшей: %s", path, exc)
            continue
        label = encoder_label(path)
        fmt = fmt or (GGUF_FORMAT if is_gguf_file(Path(path)) else SAFETENSORS_FORMAT)
        known.setdefault(salted, KnownEncoder(True, salted != unsalted, source, fmt, label))
        if unsalted != salted:
            known.setdefault(unsalted, KnownEncoder(False, True, source, fmt, label))
    return known


@dataclass(frozen=True)
class V1Verdict:
    """What happens to one v1 entry (`classify_v1_entry`).

    `action`: `migrate` (into `library_id` with `encoder` / `origin`),
    `delete` (proven pre-RoPE-fix, or uncensored and unprovable), `keep`
    (cannot be migrated: left in place, reported). `reason` is user-facing.
    """

    action: Literal["migrate", "delete", "keep"]
    reason: str
    library_id: str | None = None
    encoder: pl.EncoderProvenance | None = None
    origin: str = ""


def classify_v1_entry(metadata: Mapping[str, str], tensor: Mapping[str, Any], known: Mapping[str, KnownEncoder], *, family: str) -> V1Verdict:
    """THE v1 provenance table (pure).

    - not bfloat16 (declared or the tensor's token), an unknown width, or a bad
      length -> keep;
    - the recorded id is the PRE-FIX identity of a known encoder whose RoPE the
      loader normalizes -> delete (made under the wrong RoPE base);
    - the recorded id is a known CURRENT identity -> migrate (`migrated_v1`,
      provenance from that encoder);
    - no match and the recorded path or the family names an uncensored encoder
      -> delete (that it was made after the fix cannot be proven);
    - no match otherwise -> migrate with source `""` (`migrated_v1_unverified`).
    The format is the v1 `text_encoder_format` when it is a valid id, else the
    matched encoder's (`bf16` when unmatched).
    """
    if metadata.get("dtype") != text_encoder_dtype_name() or tensor.get("dtype") != PROMPT_CACHE_DTYPE_TOKEN:
        return V1Verdict("keep", f"тип данных {metadata.get('dtype')!r} вместо {text_encoder_dtype_name()!r}: запись надо построить заново")
    shape = tensor.get("shape")
    width = shape[-1] if isinstance(shape, list) and shape and isinstance(shape[-1], int) else None
    library_id = LEGACY_LIBRARY_BY_WIDTH.get(width) if width is not None else None
    if library_id is None:
        return V1Verdict("keep", f"неизвестная ширина эмбеддинга {width!r}: модель не определить")
    try:
        if int(metadata.get("max_sequence_length", "")) <= 0:
            raise ValueError
    except ValueError:
        return V1Verdict("keep", f"некорректная длина последовательности {metadata.get('max_sequence_length')!r}")
    recorded_id = metadata.get("text_encoder_id", "")
    recorded_path = metadata.get("text_encoder_path", "")
    fp8 = metadata.get("text_encoder_fp8", "false").strip().lower() in ("1", "true", "yes", "on")
    v1_format = metadata.get("text_encoder_format", "")
    match = known.get(recorded_id) if recorded_id else None
    if match is not None and not match.salted:
        return V1Verdict("delete", "создан до исправления RoPE энкодера: эмбеддинги неверны")
    if not recorded_id:
        return V1Verdict("keep", "в записи нет отпечатка энкодера")
    if match is not None:
        fmt = v1_format if WEIGHT_FORMAT_ID_PATTERN.fullmatch(v1_format) else match.format
        return V1Verdict("migrate", "энкодер опознан", library_id, pl.EncoderProvenance(match.source, fmt, fp8, recorded_id, match.label), pl.ORIGIN_MIGRATED_V1)
    if "uncensored" in f"{recorded_path}\n{family}".lower():
        return V1Verdict("delete", "расцензуренный энкодер не опознан: нельзя доказать, что запись сделана после исправления RoPE")
    fmt = v1_format if WEIGHT_FORMAT_ID_PATTERN.fullmatch(v1_format) else SAFETENSORS_FORMAT
    label = Path(recorded_path.rstrip("/\\")).name if recorded_path.strip("/\\") else family.rsplit("-", 1)[0]
    return V1Verdict("migrate", "энкодер не найден на этом компьютере", library_id, pl.EncoderProvenance("", fmt, fp8, recorded_id, label), pl.ORIGIN_MIGRATED_V1_UNVERIFIED)


def convert_v1_metadata(metadata: Mapping[str, str], verdict: V1Verdict, *, name: str, migrated_from: str) -> dict[str, str]:
    """The v2 `__metadata__` of a v1 entry that `verdict` migrates (the tensor bytes are kept verbatim by `PromptLibrary.add_payload`).

    `name` becomes the preset name (a v1 entry's file stem was its user-given
    name); `migrated_from` is recorded as `<family>/<file>`; the v1
    `text_encoder_path` stays as an informational field.

    # Raises
    `ValueError` when `verdict` is not a migration.
    """
    if verdict.action != "migrate" or verdict.library_id is None or verdict.encoder is None:
        raise ValueError(f"Запись не переносится: {verdict.reason}")
    extra = {"migrated_from": migrated_from}
    if metadata.get("text_encoder_path"):
        extra["text_encoder_path"] = metadata["text_encoder_path"]
    return pl.build_metadata(
        flux2_library_spec(verdict.library_id),
        prompt=metadata.get("prompt", ""),
        name=name,
        max_sequence_length=int(metadata["max_sequence_length"]),
        encoder=verdict.encoder,
        origin=verdict.origin,
        created_at=metadata.get("created_at") or None,
        extra=extra,
    )


@dataclass(frozen=True)
class MigrationReport:
    """The outcome of the v1 migration (`migrate_legacy_families`).

    `migrated`: v1 files now living as v2 entries (v1 file removed);
    `deleted`: pre-fix / unprovable files removed; `not_migrated`: files left in
    place, each in `failures` as `(<family>/<file>, reason)`; `directory`: the
    library root.
    """

    migrated: int
    deleted: int
    not_migrated: int
    directory: str
    failures: tuple[tuple[str, str], ...] = ()

    @property
    def found_anything(self) -> bool:
        return bool(self.migrated or self.deleted or self.not_migrated)

    def to_wire(self) -> dict[str, Any]:
        """The `.prompt_cache.list` `legacy` object."""
        return {"migrated": self.migrated, "deleted": self.deleted, "not_migrated": self.not_migrated, "directory": self.directory}


def legacy_family_dirs(root: Path) -> tuple[list[Path], list[tuple[str, str]]]:
    """`(families, refused)` under the real directory `root`: the v1 family directories, sorted, and the matching names that are refused.

    A name matching `LEGACY_FAMILY_PATTERN` is a family only when it is a real
    directory strictly inside `root` (`pl.require_confined`: no symbolic link,
    no reparse point); a matching link or a matching non-directory is refused
    as `(name, reason)` and never followed. Nothing when `root` does not exist.

    # Raises
    `pl.UnsafeLibraryPath` when `root` itself exists but is not a real
    directory.
    """
    if not os.path.lexists(root):
        return [], []
    pl.require_real_directory(root)
    families: list[Path] = []
    refused: list[tuple[str, str]] = []
    try:
        names = sorted(item.name for item in os.scandir(root) if LEGACY_FAMILY_PATTERN.fullmatch(item.name))
    except OSError as exc:
        log.warning("FLUX.2 klein: каталог старых кэшей промптов %s не перечислен: %s", root, exc)
        return [], []
    for name in names:
        path = root / name
        try:
            pl.require_confined(path, root, directory=True)
        except pl.UnsafeLibraryPath as exc:
            refused.append((name, str(exc)))
            continue
        families.append(path)
    return families, refused


def _family_files(family: Path, root: Path) -> tuple[list[Path], list[tuple[str, str]]]:
    """`(files, refused)` of one real family directory: its `.msprompt` regular files, sorted, and the `.msprompt` names refused as links.

    Directories and other kinds named `*.msprompt` are ignored; a link is
    refused as `(<family>/<name>, reason)` and never followed.

    # Raises
    `OSError` when the directory cannot be listed.
    """
    files: list[Path] = []
    refused: list[tuple[str, str]] = []
    with os.scandir(family) as scan:
        items = sorted(scan, key=lambda item: item.name)
    for item in items:
        if not item.name.endswith(pl.ENTRY_SUFFIX):
            continue
        try:
            info = item.stat(follow_symlinks=False)
        except OSError as exc:
            refused.append((f"{family.name}/{item.name}", str(exc)))
            continue
        if stat.S_ISDIR(info.st_mode) and not pl.is_link_like(info):
            continue
        try:
            files.append(pl.require_confined(family / item.name, root, directory=False))
        except pl.UnsafeLibraryPath as exc:
            refused.append((f"{family.name}/{item.name}", str(exc)))
    return files, refused


def _verify_migrated(library: pl.PromptLibrary, entry: pl.LibraryEntry) -> None:
    """Re-read the v2 file of `entry` from disk: a valid entry of the same identity and the stat size.

    # Raises
    `ValueError` when it is not.
    """
    path = library.path_of(entry.file)
    size = path.stat().st_size
    fresh = pl.parse_entry_header(read_safetensors_header(path), library.spec, file=entry.file, size_bytes=size, mtime_ns=0)
    if not fresh.same_identity(entry) or size <= 0:
        raise ValueError(f"перенесённая запись {path} не совпала с исходной")


def migrate_legacy_families(root: Path, extra_encoders: Iterable[tuple[str, str]]) -> MigrationReport:
    """Migrate every v1 family directory under `root` (see the module notes for the rules); idempotent.

    **Containment.** `root` must be a real directory; every family and every
    file is proven a real directory / regular file strictly inside it
    (`pl.require_confined`, no symbolic link or reparse point on the way)
    before it is read, and again right before it is deleted
    (`pl.remove_confined_file`). A linked root migrates nothing; a linked
    family or file is never followed and is reported in `failures` /
    `not_migrated`.

    **Order and durability.** Every migratable entry is written as v2 (the
    publish fsyncs the library directory), verified, the library directory
    and the root are fsynced once more (an entry that was already present may
    have been published by an older build), and only then is its v1 file
    unlinked and its family directory fsynced. The pre-fix / unprovable files
    are deleted after that (each unlink fsynced); emptied family directories
    are removed last (the root fsynced). `extra_encoders` are `(path, source)`
    pairs beside the installed packages (the request's encoder); the recorded
    v1 paths are added here.
    """
    failures: list[tuple[str, str]] = []
    try:
        families, refused = legacy_family_dirs(root)
    except pl.UnsafeLibraryPath as exc:
        log.warning("FLUX.2 klein: старые кэши промптов в %s не переносятся: %s", root, exc)
        return MigrationReport(0, 0, 1, str(root), ((root.name, str(exc)),))
    failures.extend(refused)
    found: list[tuple[Path, Path, dict[str, str] | None, dict[str, Any] | None, str]] = []
    for family in families:
        try:
            files, refused_files = _family_files(family, root)
        except OSError as exc:
            log.warning("FLUX.2 klein: старые кэши промптов в %s не перечислены: %s", family, exc)
            continue
        failures.extend(refused_files)
        for path in files:
            try:
                metadata, tensor = read_v1_header(path)
                found.append((family, path, metadata, tensor, ""))
            except ValueError as exc:
                found.append((family, path, None, None, str(exc)))
    if not found and not failures:
        return MigrationReport(0, 0, 0, str(root))
    recorded = [(metadata.get("text_encoder_path", ""), "") for _family, _path, metadata, _tensor, _error in found if metadata is not None]
    known = known_local_encoders(list(extra_encoders) + recorded)
    migrated = deleted = 0
    to_delete: list[tuple[str, Path, str]] = []
    for family, path, metadata, tensor, error in found:
        where = f"{family.name}/{path.name}"
        if metadata is None or tensor is None:
            failures.append((where, error))
            continue
        verdict = classify_v1_entry(metadata, tensor, known, family=family.name)
        if verdict.action == "keep":
            failures.append((where, verdict.reason))
        elif verdict.action == "delete":
            to_delete.append((where, path, verdict.reason))
        else:
            try:
                library = library_of(str(verdict.library_id))
                v2 = convert_v1_metadata(metadata, verdict, name=path.stem, migrated_from=where)
                entry, _new = library.add_payload(pl.require_confined(path, root, directory=False).read_bytes(), v2)
                _verify_migrated(library, entry)
                # The v2 entry must be durable BEFORE its only other copy goes.
                pl.sync_directory(library.directory)
                pl.sync_directory(root)
                pl.remove_confined_file(path, root)
            except (OSError, ValueError) as exc:
                log.warning("FLUX.2 klein: старый кэш промпта %s не перенесён: %s", where, exc)
                failures.append((where, str(exc)))
                continue
            migrated += 1
            log.info("FLUX.2 klein: старый кэш промпта %s перенесён в библиотеку %s как %s (%s).", where, verdict.library_id, entry.file, verdict.reason)
    for where, path, reason in to_delete:
        try:
            pl.remove_confined_file(path, root)
        except (OSError, ValueError) as exc:
            log.warning("FLUX.2 klein: старый кэш промпта %s не удалён: %s", where, exc)
            failures.append((where, str(exc)))
            continue
        deleted += 1
        log.info("FLUX.2 klein: старый кэш промпта %s удалён: %s.", where, reason)
    for family in families:
        try:
            if not any(family.iterdir()):
                pl.remove_confined_empty_directory(family, root)
                log.info("FLUX.2 klein: пустой каталог старых кэшей %s удалён.", family)
        except (OSError, ValueError) as exc:
            log.debug("FLUX.2 klein: каталог %s не удалён: %s", family, exc)
    for where, reason in failures:
        log.warning("FLUX.2 klein: старый кэш промпта %s оставлен на месте: %s", where, reason)
    return MigrationReport(migrated, deleted, len(failures), str(root), tuple(failures))


_MIGRATION_LOCK = threading.Lock()
_MIGRATION_REPORTS: dict[str, MigrationReport] = {}


def ensure_legacy_migrated(extra_encoders: Iterable[tuple[str, str]]) -> MigrationReport:
    """Run `migrate_legacy_families` over `prompt_cache_root()` once per process (per root) and return its report.

    Called by the library operations (`.list`, `.build`, `.import`, a run that
    names a library), never by `.status` or `large_models.profiles`.
    """
    root = prompt_cache_root()
    key = os.path.abspath(str(root))
    with _MIGRATION_LOCK:
        report = _MIGRATION_REPORTS.get(key)
        if report is None:
            report = migrate_legacy_families(root, extra_encoders)
            _MIGRATION_REPORTS[key] = report
        return report


def migration_report() -> MigrationReport | None:
    """The report of this process's migration of `prompt_cache_root()`, `None` when it has not run."""
    return _MIGRATION_REPORTS.get(os.path.abspath(str(prompt_cache_root())))
