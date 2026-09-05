"""
File: modules/ai_backend/inpaint/flux2_klein/components.py

Purpose:
Everything about the five pipeline components AS FILES ON DISK: where they are
searched for, how the two components the user does not supply (tokenizer,
scheduler) are discovered next to the three they do, how a transformer
checkpoint is inspected and rejected, how large a component is, and how all of
that is shaped into the `components` map `status` returns.

Main responsibilities:
- search roots and probe order (`component_search_roots`, `component_probe_order`,
  `discover_component_dir`, `_require_component_dir`);
- safetensors header reading and the fp8-scaled refusal
  (`read_safetensors_header`, `is_fp8_scaled_checkpoint`, `_reject_fp8_scaled_directory`);
- the transformer config contract (`find_transformer_config_dir`,
  `validate_transformer_config_dir`, `_missing_transformer_config_message`);
- text-encoder availability (`text_encoder_available`, `require_text_encoder`);
- the encoder<->transformer width contract (`TEXT_ENCODER_OUT_LAYERS`,
  `transformer_config_dir`, `require_encoder_transformer_compatible`) — the one
  check that tells a 9B component pair from a 4B one before any weight is read;
- on-disk size and per-path/per-component state for `status`
  (`_weight_bytes`, `_path_state`, `_component_states`, `_first_unavailable_reason`).

Notes:
- `_weight_bytes` and `is_torch_available` are replaced by the test suite through
  this module object, so every consumer outside this file must reach them as
  `components._weight_bytes(...)` / through this module rather than by importing
  the name - one patch must reach every caller.
- No torch import: this module answers questions about the filesystem only.
"""

from __future__ import annotations

import json
import logging
import struct
from pathlib import Path
from typing import Any

from ...runtime.torch_support import is_torch_available

log = logging.getLogger(__name__)

#: Subdirectory names searched for the two components the user does not supply.
_TOKENIZER_SUBDIR = "tokenizer"
_SCHEDULER_SUBDIR = "scheduler"

#: Subdirectory searched for the transformer's own `config.json` when the user
#: supplied the transformer as a single `.safetensors` file.
_TRANSFORMER_SUBDIR = "transformer"

#: `_class_name` a discovered transformer `config.json` must carry, when it
#: carries one at all. A config of another component (a VAE, a text encoder)
#: found next to the checkpoint would otherwise build a different architecture.
_TRANSFORMER_CONFIG_CLASS_NAME = "Flux2Transformer2DModel"

#: A directory is a tokenizer when it carries one of these files.
_TOKENIZER_MARKERS = ("tokenizer.json", "tokenizer_config.json")

#: A directory is a scheduler when it carries this file.
_SCHEDULER_MARKER = "scheduler_config.json"

#: A directory is a diffusers/transformers model when it carries this file.
_MODEL_CONFIG_MARKER = "config.json"

#: Upper bound on the JSON header of a safetensors file we are willing to parse.
_MAX_SAFETENSORS_HEADER_BYTES = 100 * 1024 * 1024

#: Suffixes of the per-tensor scale entries an fp8-scaled checkpoint carries.
#: diffusers 0.39 has no converter for them, so such a file must be rejected with
#: a readable message instead of failing deep inside the loader.
_FP8_SCALE_SUFFIXES = (".weight_scale", ".input_scale", ".scale_weight", ".scale_input")

#: safetensors dtype tokens for the two 8-bit float formats.
_FP8_DTYPES = ("F8_E4M3", "F8_E5M2")

#: Hidden-state layers of the text encoder the klein pipeline concatenates
#: (`Flux2KleinInpaintPipeline._get_qwen_prompt_embeds`, layers `(9, 18, 27)`).
#: The prompt embedding is therefore this many times the encoder's `hidden_size`
#: wide, which is what must equal the transformer's `joint_attention_dim` —
#: see `require_encoder_transformer_compatible`.
TEXT_ENCODER_OUT_LAYERS = 3


# =====================================================================
#  Component discovery (tokenizer / scheduler / transformer config)
# =====================================================================
def component_search_roots(normalized_or_paths: dict[str, Any]) -> list[Path]:
    """Directories searched for the components the user does not supply.

    Order (first match wins): the text-encoder directory and its parent, then the
    transformer directory and its parent, then the VAE directory and its parent.
    A klein checkout keeps `text_encoder/`, `tokenizer/`, `scheduler/`, `vae/` and
    `transformer/` side by side, so the parent of any supplied path is usually
    the repository root.
    """
    roots: list[Path] = []
    for key in ("text_encoder_path", "transformer_path", "vae_path"):
        raw = str(normalized_or_paths.get(key) or "").strip()
        if not raw:
            continue
        candidate = Path(raw)
        base = candidate.parent if candidate.is_file() else candidate
        for root in (base, base.parent):
            if root not in roots:
                roots.append(root)
    return roots


def component_probe_order(roots: list[Path], subdir: str) -> list[Path]:
    """Directories `discover_component_dir` probes for `subdir`, in search order.

    Pure path arithmetic — nothing is touched on disk. It exists so that an error
    message listing "we looked here" cannot drift away from where the search
    actually looked; both go through this function.
    """
    order: list[Path] = []
    for root in roots:
        for candidate in (root / subdir, root):
            if candidate not in order:
                order.append(candidate)
    return order


def discover_component_dir(roots: list[Path], subdir: str, markers: tuple[str, ...]) -> Path | None:
    """First directory under `roots` that holds one of `markers`.

    Both `<root>/<subdir>` and `<root>` itself are probed, because a
    transformers-style text-encoder folder carries its tokenizer files directly
    while a diffusers checkout keeps them in a sibling subfolder.
    """
    for candidate in component_probe_order(roots, subdir):
        try:
            if candidate.is_dir() and any((candidate / m).is_file() for m in markers):
                return candidate
        except OSError:
            continue
    return None


def _require_component_dir(
    roots: list[Path], subdir: str, markers: tuple[str, ...], human_name: str
) -> Path:
    """`discover_component_dir` or an explicit error naming where to put it."""
    found = discover_component_dir(roots, subdir, markers)
    if found is not None:
        return found
    searched = ", ".join(str(root) for root in roots) or "(пути не заданы)"
    raise FileNotFoundError(
        f"Не найден {human_name} для FLUX.2 klein. Ожидается каталог с файлом "
        f"{markers[0]} — положите его как «{subdir}» рядом с текстовым энкодером или "
        f"трансформером. Просмотрены каталоги: {searched}"
    )


# =====================================================================
#  Checkpoint inspection
# =====================================================================
def read_safetensors_header(path: Path) -> dict[str, Any]:
    """Parse the JSON header of a safetensors file without loading any tensor.

    # Raises
    `ValueError` when the file is not a safetensors container or its header is
    implausibly large / not valid UTF-8 JSON.
    """
    try:
        with path.open("rb") as handle:
            raw_len = handle.read(8)
            if len(raw_len) != 8:
                raise ValueError(f"Файл слишком мал для safetensors: {path}")
            header_len = struct.unpack("<Q", raw_len)[0]
            if header_len == 0 or header_len > _MAX_SAFETENSORS_HEADER_BYTES:
                raise ValueError(
                    f"Некорректный заголовок safetensors ({header_len} байт): {path}"
                )
            payload = handle.read(header_len)
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {path}: {exc}") from exc
    if len(payload) != header_len:
        raise ValueError(f"Обрезанный заголовок safetensors: {path}")
    try:
        header = json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"Заголовок safetensors не является JSON: {path}") from exc
    if not isinstance(header, dict):
        raise ValueError(f"Заголовок safetensors не является объектом: {path}")
    return header


def is_fp8_scaled_checkpoint(header: dict[str, Any]) -> bool:
    """Whether a safetensors header describes an fp8_scaled checkpoint.

    Such files carry per-tensor `weight_scale`/`input_scale` entries and/or
    8-bit float tensors. diffusers 0.39 has no converter for that layout, so the
    caller must reject the file with a readable message instead of letting the
    single-file loader fail on unknown keys.
    """
    for key, entry in header.items():
        if key == "__metadata__":
            continue
        if key.endswith(_FP8_SCALE_SUFFIXES):
            return True
        if isinstance(entry, dict) and str(entry.get("dtype", "")).upper() in _FP8_DTYPES:
            return True
    return False


def _fp8_scaled_message(source: Path) -> str:
    """The single wording used to refuse an fp8_scaled file or shard."""
    return (
        f"Чекпоинт {source.name} сохранён в формате fp8_scaled (тензоры *_scale). "
        "diffusers 0.39 не умеет его конвертировать — используйте bf16/fp16 "
        "safetensors или каталог в формате diffusers."
    )


def component_safetensors_shards(source: Path) -> list[Path]:
    """Every safetensors shard belonging to the diffusers folder `source`.

    A sharded checkout names its parts in a `*.index.json` weight map, which may
    point at names the directory glob would order differently, so the index is
    consulted first and the plain `*.safetensors` glob fills in the rest. An
    unreadable or malformed index is skipped rather than raised on: it is only a
    hint here, and the real loader gives a better message for a broken checkout.
    """
    shards: list[Path] = []
    seen: set[Path] = set()
    for index_path in sorted(source.glob("*.index.json")):
        try:
            data = json.loads(index_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        weight_map = data.get("weight_map") if isinstance(data, dict) else None
        if not isinstance(weight_map, dict):
            continue
        for name in weight_map.values():
            candidate = source / str(name)
            if candidate.is_file() and candidate not in seen:
                seen.add(candidate)
                shards.append(candidate)
    for candidate in sorted(source.glob("*.safetensors")):
        if candidate not in seen:
            seen.add(candidate)
            shards.append(candidate)
    return shards


def transformer_config_roots(source: Path) -> list[Path]:
    """Roots searched for the transformer `config.json` next to a single file.

    The checkpoint's own directory first, then its parent: a klein checkout keeps
    `transformer/config.json` beside the checkpoint, while a user who dropped the
    file inside `transformer/` is one level deeper. Deduplicated, because at the
    filesystem root a directory is its own parent.
    """
    roots: list[Path] = []
    for root in (source.parent, source.parent.parent):
        if root not in roots:
            roots.append(root)
    return roots


def find_transformer_config_dir(source: Path) -> Path | None:
    """Directory holding the transformer `config.json` for checkpoint `source`.

    `None` when there is none: the caller must refuse the load, because the
    parameters that config carries cannot be recovered from the weights (see
    `_missing_transformer_config_message`).
    """
    return discover_component_dir(
        transformer_config_roots(source), _TRANSFORMER_SUBDIR, (_MODEL_CONFIG_MARKER,)
    )


def validate_transformer_config_dir(config_dir: Path) -> None:
    """Refuse a discovered `config.json` that belongs to a different model class.

    The search also probes the checkpoint's own directory, so a file sitting in a
    VAE or text-encoder folder would otherwise hand `from_single_file` that
    component's config and build the wrong architecture. A config without a
    `_class_name` (a hand-written one) is accepted, since there is nothing to
    contradict.

    # Raises
    `ValueError` when the file is unreadable, is not a JSON object, or names a
    class other than `Flux2Transformer2DModel`.
    """
    path = config_dir / _MODEL_CONFIG_MARKER
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"Не удалось прочитать конфиг трансформера {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ValueError(f"Конфиг трансформера не является JSON-объектом: {path}")
    class_name = data.get("_class_name")
    if isinstance(class_name, str) and class_name != _TRANSFORMER_CONFIG_CLASS_NAME:
        raise ValueError(
            f"Файл {path} — конфиг «{class_name}», а не «{_TRANSFORMER_CONFIG_CLASS_NAME}». "
            "Рядом с чекпоинтом трансформера должен лежать каталог «transformer» с его "
            "собственным config.json."
        )
    if class_name is None:
        log.debug("FLUX.2 klein: конфиг трансформера %s без _class_name, принят как есть", path)


def _missing_transformer_config_message(source: Path) -> str:
    """The refusal shown when a single-file transformer has no config beside it.

    States the remedy (which file, from which model, in which directory) and the
    reason there is no fallback: `rope_theta`, `eps` and `patch_size` are not
    derivable from the weights, and a wrong value for any of them produces a
    model whose tensor shapes are IDENTICAL — nothing would catch it before the
    user got quietly wrong images.
    """
    searched = ", ".join(
        str(candidate)
        for candidate in component_probe_order(transformer_config_roots(source), _TRANSFORMER_SUBDIR)
    )
    expected = source.parent / _TRANSFORMER_SUBDIR / _MODEL_CONFIG_MARKER
    return (
        f"Не найден config.json трансформера для FLUX.2 klein: одиночный файл {source.name} "
        "не содержит конфигурации.\n"
        f"Что сделать: положите config.json трансформера ОТ ЭТОЙ ЖЕ модели как «{expected}».\n"
        "Почему без него нельзя: diffusers 0.39 распознаёт любой чекпоинт FLUX.2 как "
        "flux-2-dev и взял бы конфиг закрытого репозитория black-forest-labs/FLUX.2-dev — "
        "конфиг ДРУГОЙ модели; этот путь заблокирован намеренно. Подобрать параметры самим "
        "тоже нельзя: rope_theta, eps и patch_size не выводятся из весов, а модель с неверным "
        "rope_theta имеет те же формы тензоров и молча выдаёт неправильные изображения.\n"
        f"Просмотрены каталоги: {searched}"
    )


def _reject_fp8_scaled_directory(source: Path) -> None:
    """Refuse a diffusers transformer folder whose shards are fp8_scaled.

    The single-file path reads the header and refuses before loading anything;
    a folder must get the same treatment, otherwise `from_pretrained` starts a
    multi-GiB load and fails deep inside the loader with an unrelated message
    after the RAM and I/O have already been spent.

    # Raises
    `ValueError` naming the first fp8_scaled shard found.
    """
    for shard in component_safetensors_shards(source):
        try:
            header = read_safetensors_header(shard)
        except ValueError:
            # Not a readable safetensors container: it says nothing about the
            # layout, and the real loader reports it better than we could.
            continue
        if is_fp8_scaled_checkpoint(header):
            raise ValueError(_fp8_scaled_message(shard))


# =====================================================================
#  Text-encoder availability
# =====================================================================
def text_encoder_available(paths: dict[str, Any]) -> bool:
    """Whether a text encoder is present ON THIS MACHINE for `paths`.

    `paths` may be a normalized parameter dict or the lenient `_lenient_paths`
    map; only `text_encoder_path` is read. The answer is `False` both for an
    empty path and for one that does not exist, because those are the same
    situation from the run's point of view: nothing here can encode a prompt.
    A `.msprompt` file loaded into the cache is what makes a run possible
    anyway — see `normalize_flux2_klein_params`.

    It says nothing about whether the directory is a USABLE encoder; that is
    `text_encoder_fingerprint`'s job, and it is asked only when this returns
    `True`.
    """
    raw = str(paths.get("text_encoder_path") or "").strip()
    return bool(raw) and Path(raw).exists()


def require_text_encoder(normalized: dict[str, Any], *, what: str) -> None:
    """Refuse an operation that genuinely needs the encoder when there is none.

    THE single point where a missing encoder becomes an error for anything that
    must ENCODE — called both early (before a run reads 18 GB of transformer it
    would have to throw away) and at the encode itself
    (`_encode_prompts_locked`, which cannot be reached any other way). `what`
    names the operation in the message. A SAVE needs the encoder for its identity
    rather than for an encode and refuses with its own wording, in
    `Flux2KleinInpaintService._require_current_family`.

    The message states BOTH ways out on purpose: the user either points the
    settings at an encoder, or loads a ready `.msprompt` for this exact prompt.
    Naming only the first would tell a user who cannot download 16 GB that the
    feature is closed to them, when it is not.

    # Raises
    `ValueError` when no local encoder is available.
    """
    if text_encoder_available(normalized):
        return
    configured = str(normalized.get("text_encoder_path") or "").strip()
    where = f" (путь не найден: {configured})" if configured else ""
    raise ValueError(
        f"Текстовый энкодер недоступен{where}, а {what} требует кодирования промпта. "
        "Либо укажите каталог текстового энкодера в настройках, либо загрузите готовый кэш "
        "промпта для этого текста (inpaint.flux2_klein.prompt_cache.load) — с ним генерация "
        "энкодер не читает."
    )


def component_dir_for_path(path: Path) -> Path:
    """Directory of a diffusers component, given a folder OR a file inside it.

    `AutoencoderKLFlux2` and the Qwen3 encoder have no single-file loader, so a
    weights file is only usable through the folder that carries its
    `config.json`. Users pick the `.safetensors` file far more often than its
    folder, and the folder holding a file IS the component, so the file is
    normalized to its parent instead of being refused. Anything else (a folder,
    or a file with no sibling config) is returned unchanged, leaving the caller
    to produce its own diagnosis.
    """
    if path.is_file() and (path.parent / _MODEL_CONFIG_MARKER).is_file():
        return path.parent
    return path


# =====================================================================
#  Encoder <-> transformer compatibility
# =====================================================================
def _read_json_object(path: Path) -> dict[str, Any] | None:
    """Parse `path` as a JSON object, or `None` when it cannot be read as one.

    Deliberately silent about failure: the only caller is a guard that speaks up
    when it can PROVE an incompatibility, and a missing or malformed config is a
    different fault with a much better diagnosis of its own further down
    (`_missing_transformer_config_message`, the loaders themselves).
    """
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        log.debug("FLUX.2 klein: не удалось прочитать %s (%s)", path, exc)
        return None
    return data if isinstance(data, dict) else None


def _config_int(data: dict[str, Any] | None, key: str) -> int | None:
    """A positive integer field of a model config, or `None` when it is absent.

    `bool` is rejected explicitly because it is a subclass of `int` and a `true`
    in a hand-edited config would otherwise read as the dimension `1`.
    """
    if data is None:
        return None
    value = data.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        return None
    return value


def transformer_config_dir(transformer_path: str) -> Path | None:
    """Directory holding the transformer `config.json` for a folder OR a single file.

    A diffusers folder carries its own `config.json`; a single `.safetensors`
    checkpoint has one found beside it by `find_transformer_config_dir`, which
    is the SAME search `_load_transformer` performs, so the guard below and the
    loader always read the same file. `None` when there is none to read.
    """
    source = Path(transformer_path)
    try:
        if source.is_dir():
            return source if (source / _MODEL_CONFIG_MARKER).is_file() else None
        if not source.is_file():
            return None
    except OSError:
        return None
    return find_transformer_config_dir(source)


def require_encoder_transformer_compatible(paths: dict[str, Any]) -> None:
    """Refuse a text encoder whose width does not match the transformer's.

    **The contract.** `Flux2KleinInpaintPipeline._get_qwen_prompt_embeds`
    (diffusers 0.39, `pipeline_flux2_klein.py`) stacks the text encoder's hidden
    states at layers `(9, 18, 27)` and reshapes them to
    `(batch, seq, TEXT_ENCODER_OUT_LAYERS * hidden_size)`; the transformer feeds
    that straight into `context_embedder = nn.Linear(joint_attention_dim, ...)`
    (`transformer_flux2.py`). So the two components fit if and only if
    `TEXT_ENCODER_OUT_LAYERS * text_encoder.hidden_size == joint_attention_dim`
    — 3 * 4096 == 12288 for the 9B variant, 3 * 2560 == 7680 for the 4B one.

    **Why it is checked here rather than left to torch.** Nothing else compares
    the two. A 4B transformer beside a 9B encoder passes every existing check,
    passes the memory guard, and dies as a bare matmul shape error inside the
    denoise — after ~34 GB has been read from disk. This reads two `config.json`
    files, never a weight, so it costs nothing and it fires first.

    `paths` is a normalized parameter dict or the lenient `_lenient_paths` map;
    only `text_encoder_path` and `transformer_path` are read. The guard is
    SILENT whenever it cannot prove a mismatch — an empty path, a component that
    is not on disk, a config without the field — because absence is a different
    fault and every one of those has a better diagnosis of its own downstream.

    # Raises
    `ValueError` naming both config files, both numbers and the remedy.
    """
    encoder_raw = str(paths.get("text_encoder_path") or "").strip()
    transformer_raw = str(paths.get("transformer_path") or "").strip()
    if not encoder_raw or not transformer_raw:
        return

    config_dir = transformer_config_dir(transformer_raw)
    if config_dir is None:
        return
    transformer_config = config_dir / _MODEL_CONFIG_MARKER
    encoder_config = component_dir_for_path(Path(encoder_raw)) / _MODEL_CONFIG_MARKER

    joint = _config_int(_read_json_object(transformer_config), "joint_attention_dim")
    hidden = _config_int(_read_json_object(encoder_config), "hidden_size")
    if joint is None or hidden is None:
        log.debug(
            "FLUX.2 klein: совместимость энкодера и трансформера не проверена — "
            "joint_attention_dim=%s (%s), hidden_size=%s (%s)",
            joint,
            transformer_config,
            hidden,
            encoder_config,
        )
        return

    produced = hidden * TEXT_ENCODER_OUT_LAYERS
    if produced == joint:
        return

    log.error(
        "FLUX.2 klein: несовместимые компоненты — трансформер ждёт %d каналов текста, "
        "энкодер даёт %d.\n"
        "Конфиг трансформера: %s (joint_attention_dim=%d)\n"
        "Конфиг энкодера: %s (hidden_size=%d, слоёв склейки: %d)\n"
        "Вероятная причина: трансформер и энкодер взяты из разных вариантов модели "
        "(FLUX.2-klein-9B и FLUX.2-klein-4B).",
        joint,
        produced,
        transformer_config,
        joint,
        encoder_config,
        hidden,
        TEXT_ENCODER_OUT_LAYERS,
    )
    # The hint is only true when the width really divides: naming
    # `joint // 3` for a `joint_attention_dim` that is not a multiple of three
    # would send the user looking for an encoder that cannot exist.
    expected = ""
    if joint % TEXT_ENCODER_OUT_LAYERS == 0:
        expected = f" — нужен энкодер с hidden_size {joint // TEXT_ENCODER_OUT_LAYERS}"
    raise ValueError(
        "Текстовый энкодер не подходит к трансформеру FLUX.2 klein: это компоненты разных "
        "вариантов модели.\n"
        f"Трансформер: {transformer_config}\n"
        f"  joint_attention_dim = {joint}{expected}.\n"
        f"Текстовый энкодер: {encoder_config}\n"
        f"  hidden_size = {hidden}; пайплайн склеивает {TEXT_ENCODER_OUT_LAYERS} слоя энкодера "
        f"и даёт {produced}.\n"
        "Что сделать: возьмите трансформер и текстовый энкодер из ОДНОГО каталога модели — "
        "FLUX.2-klein-9B и FLUX.2-klein-4B между собой несовместимы."
    )


def _weight_bytes(path: str) -> int:
    """On-disk size of a component: one file, or every weight file in a folder.

    A bf16/fp16 checkpoint stores two bytes per parameter, which is also what it
    occupies once loaded, so the file size doubles as the weight-memory estimate.
    """
    source = Path(path)
    try:
        if source.is_file():
            return int(source.stat().st_size)
        if not source.is_dir():
            return 0
        total = 0
        for entry in source.rglob("*"):
            if entry.is_file() and entry.suffix.lower() in (".safetensors", ".bin", ".pt", ".pth"):
                total += int(entry.stat().st_size)
        return total
    except OSError as exc:
        log.debug("FLUX.2 klein: could not size %s (%s)", path, exc)
        return 0


def _path_state(path: str | None) -> dict[str, Any]:
    """`{path, exists, size_bytes}` for one user-supplied component path."""
    raw = str(path or "").strip()
    if not raw:
        return {"path": "", "exists": False, "size_bytes": 0}
    return {"path": raw, "exists": Path(raw).exists(), "size_bytes": _weight_bytes(raw)}


def _component_states(paths: dict[str, str]) -> dict[str, Any]:
    """The five `status.components` entries built from the paths alone.

    Disk facts only, and deliberately lock-free: the three user-supplied paths
    become `{path, exists, size_bytes}` and the two discovered directories become
    `{found, path}`. The residency of the components that are actually loaded is
    merged on top of this by `Flux2KleinInpaintService.status`, which is the part
    that needs the service lock.
    """
    roots = component_search_roots(paths)
    tokenizer_dir = discover_component_dir(roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS)
    scheduler_dir = discover_component_dir(roots, _SCHEDULER_SUBDIR, (_SCHEDULER_MARKER,))
    return {
        "text_encoder": _path_state(paths.get("text_encoder_path")),
        "transformer": _path_state(paths.get("transformer_path")),
        "vae": _path_state(paths.get("vae_path")),
        "tokenizer": {
            "found": tokenizer_dir is not None,
            "path": str(tokenizer_dir) if tokenizer_dir is not None else "",
        },
        "scheduler": {
            "found": scheduler_dir is not None,
            "path": str(scheduler_dir) if scheduler_dir is not None else "",
        },
    }


def _first_unavailable_reason(components: dict[str, Any], *, prompt_cached: bool) -> str | None:
    """First blocking reason for `status.available`, or `None` when all is well.

    `prompt_cached` says a READY embedding exists for the request in hand, and
    that changes what "available" means: the encode phase is then skipped
    entirely, so the text encoder is never read and must not block the run.
    Reporting `available: false` because a 16 GB encoder is absent would hide a
    run that works — which is the whole point of carrying a `.msprompt` to a
    machine that never downloaded it.

    **The Qwen tokenizer is NOT waived along with it**, even though the denoise
    never tokenizes anything: `_ensure_pipeline_locked` builds the pipeline with
    a real `Qwen2TokenizerFast` and `_require_component_dir` raises without one,
    so a run on a machine that has no `tokenizer/` fails at the load. Verified on
    the reference host — the pipeline build emits its own "Загрузка токенизатора"
    step. The transformer, the VAE and the scheduler are needed in every case
    too.
    """
    if not is_torch_available():
        return "PyTorch не установлен"
    if not prompt_cached:
        encoder = components["text_encoder"]
        # Both branches name the cache as the second way out: this is exactly the
        # state a machine is in when a `.msprompt` was copied to it but not loaded
        # yet, and the settings still carry the other machine's encoder path.
        if not encoder["path"]:
            return (
                "Не выбран текстовый энкодер (или загрузите готовый кэш промпта — "
                "с ним энкодер не нужен)"
            )
        if not encoder["exists"]:
            return (
                f"Путь текстового энкодера не найден: {encoder['path']} "
                "(или загрузите готовый кэш промпта — с ним энкодер не нужен)"
            )
    for name, human in (("transformer", "трансформер"), ("vae", "VAE")):
        entry = components[name]
        if not entry["path"]:
            return f"Не выбран {human}"
        if not entry["exists"]:
            return f"Путь не найден: {entry['path']}"
    if not components["tokenizer"]["found"]:
        return "Не найден токенизатор Qwen (каталог «tokenizer» рядом с моделью)"
    if not components["scheduler"]["found"]:
        return "Не найден планировщик (каталог «scheduler» с scheduler_config.json)"
    return None
