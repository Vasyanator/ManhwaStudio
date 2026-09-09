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
  (`read_safetensors_header_and_data_start`, `read_safetensors_header`,
  `is_fp8_scaled_checkpoint`, `_reject_fp8_scaled_directory`);
- the transformer config contract (`find_transformer_config_dir`,
  `validate_transformer_config_dir`, `_missing_transformer_config_message`);
- text-encoder availability (`text_encoder_available`, `require_text_encoder`);
- the encoder<->transformer width contract (`TEXT_ENCODER_OUT_LAYER_INDICES`,
  `TEXT_ENCODER_OUT_LAYERS`, `transformer_config_dir`,
  `require_encoder_transformer_compatible`) — the one check that tells a 9B
  component pair from a 4B one before any weight is read;
- what the checkpoint's own `model_index.json` declares about guidance
  distillation (`model_index_search_roots`, `checkpoint_is_distilled`) — a
  tri-state, because a hand-assembled component folder declares nothing;
- the text encoder's truncation to the layers the pipeline actually reads
  (`ENCODER_KEEP_LAYERS`, `text_encoder_truncation_kwargs`) and the resident size
  that follows from it (`text_encoder_resident_bytes`);
- the VAE's own tiling threshold (`vae_tile_threshold_pixels`,
  `VAE_TILE_THRESHOLD_FALLBACK_PIXELS`) — the pixel side above which diffusers
  actually decodes tiled, which is what the memory forecast must know before it
  credits `vae_tiling` with a saving;
- on-disk size and per-path/per-component state for `status`
  (`_weight_bytes`, `_path_state`, `_component_states`, `_first_unavailable_reason`).

Notes:
- `_weight_bytes` is the DISK size (`status.components[*].size_bytes`) and
  `text_encoder_resident_bytes` is the RAM size the forecast needs. They differ
  for the text encoder alone, and on purpose; neither may be expressed in terms
  of the other.
- `_weight_bytes`, `text_encoder_resident_bytes` and `is_torch_available` are
  replaced by the test suite through this module object, so every consumer
  outside this file must reach them as `components._weight_bytes(...)` /
  `components.text_encoder_resident_bytes(...)` / through this module rather than
  by importing the name - one patch must reach every caller.
- No torch import: this module answers questions about the filesystem only.
"""

from __future__ import annotations

import json
import logging
import struct
from pathlib import Path
from typing import Any

from ...runtime.torch_support import is_torch_available
from .params import MAX_REGION_PIXELS

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

#: Hidden-state INDICES of the text encoder the klein inpaint pipeline
#: concatenates. This must equal the default of
#: `Flux2KleinInpaintPipeline.encode_prompt`
#: (`diffusers/pipelines/flux2/pipeline_flux2_klein_inpaint.py`,
#: `text_encoder_out_layers: tuple[int] = (9, 18, 27)`), which is a diffusers
#: default we do NOT own — the sibling `pipeline_flux2.py` uses `(10, 20, 30)`
#: for the non-klein FLUX.2 pipeline, so the two are genuinely different models'
#: conventions and an upstream edit could move ours. `_encode_prompt_phase`
#: therefore passes this tuple EXPLICITLY rather than inheriting the default:
#: `ENCODER_KEEP_LAYERS` below is computed from it, and a silent upstream change
#: would otherwise truncate away a layer the pipeline still reads.
TEXT_ENCODER_OUT_LAYER_INDICES = (9, 18, 27)

#: How many hidden states the klein pipeline concatenates. Derived, never a
#: second literal: the prompt embedding is this many times the encoder's
#: `hidden_size` wide, which is what must equal the transformer's
#: `joint_attention_dim` — see `require_encoder_transformer_compatible`.
TEXT_ENCODER_OUT_LAYERS = len(TEXT_ENCODER_OUT_LAYER_INDICES)

#: Decoder layers the text encoder must keep for the run, i.e. how far
#: `_load_text_encoder` truncates it. Everything above this index is dead weight:
#: the pipeline reads `output.hidden_states` and nothing else, so layers past the
#: last requested index are computed, stored and thrown away.
#:
#: **The `+ 1` is exact and must not be shaved.** `hidden_states[n]` for
#: `n == num_hidden_layers` is not a raw layer output but the output of
#: `model.norm` applied to the last layer — measured on a random Qwen3, a 6-layer
#: and a 4-layer model agree bit-for-bit on `hidden_states[1..3]` and differ by
#: 2.53 on `hidden_states[4]`. Keeping `max(indices)` layers instead of
#: `max(indices) + 1` would therefore silently feed the denoise a normed tensor
#: where a raw one belongs. Keeping MORE than this changes nothing but the cost:
#: with 36 layers the states at `(9, 18, 27)` are bit-identical to a 28-layer
#: model's, which is why embeddings saved before the truncation stay valid.
ENCODER_KEEP_LAYERS = max(TEXT_ENCODER_OUT_LAYER_INDICES) + 1

#: Prefix of the tied output projection of a causal Qwen3. `Qwen3Model` (the
#: class this service loads) has no such layer at all, so its tensors are never
#: materialized — see `text_encoder_resident_bytes`.
_ENCODER_LM_HEAD_PREFIX = "lm_head."

#: Prefix of one decoder layer's tensors in a Qwen3 checkpoint, e.g.
#: `model.layers.31.self_attn.q_proj.weight`.
_ENCODER_LAYER_PREFIX = "model.layers."

#: File suffixes counted as component weights. Shared by `_weight_bytes` and
#: `text_encoder_resident_bytes` so the disk walk and the resident walk cannot
#: disagree about which files are weights.
_WEIGHT_SUFFIXES = (".safetensors", ".bin", ".pt", ".pth")

#: Tiling threshold `vae_tile_threshold_pixels` reports when the VAE's own config
#: cannot answer. It is `params.MAX_REGION_PIXELS` used as a SIDE, which no region
#: can ever reach: that constant caps the region's AREA, so a side is at most
#: `sqrt(MAX_REGION_PIXELS * MAX_REGION_ASPECT_RATIO)` — under 3000 px. An
#: unreadable config therefore means "tiling will not engage", which is the
#: EXPENSIVE answer, and that direction is deliberate: the forecast feeds a guard
#: whose failure mode is the kernel OOM killer.
VAE_TILE_THRESHOLD_FALLBACK_PIXELS = MAX_REGION_PIXELS


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
def read_safetensors_header_and_data_start(path: Path) -> tuple[dict[str, Any], int]:
    """Parse the JSON header AND report where the tensor data section begins.

    The second element is the absolute byte offset of the first tensor byte,
    i.e. `8 + header_len`: every `data_offsets` pair in the header is relative
    to it. A streaming reader needs both halves, so this is the full answer and
    `read_safetensors_header` is the header-only view of it — there is exactly
    one parser, and its `_MAX_SAFETENSORS_HEADER_BYTES` ceiling and truncation
    checks apply to both callers.

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
    return header, 8 + header_len


def read_safetensors_header(path: Path) -> dict[str, Any]:
    """Parse the JSON header of a safetensors file without loading any tensor.

    A thin view over `read_safetensors_header_and_data_start` for the callers
    that only ask what the file CONTAINS, not where it keeps it.

    # Raises
    `ValueError` when the file is not a safetensors container or its header is
    implausibly large / not valid UTF-8 JSON.
    """
    return read_safetensors_header_and_data_start(path)[0]


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


# =====================================================================
#  Checkpoint metadata: guidance distillation
# =====================================================================
#: The diffusers pipeline manifest at a checkpoint root. It is not one of the
#: three paths the user supplies, so it is DISCOVERED beside them, exactly as the
#: tokenizer and the scheduler are.
_MODEL_INDEX_MARKER = "model_index.json"


def model_index_search_roots(paths: dict[str, Any]) -> list[Path]:
    """Directories probed for `model_index.json`, in search order.

    Deliberately NARROWER than `component_search_roots`: only the transformer and
    the VAE contribute a root. `model_index.json` describes the diffusion
    pipeline, and the `is_distilled` read from it becomes part of what the
    resident pipeline IS — so it must be a function of the paths
    `params._model_key` ALWAYS carries. The text-encoder path enters that key
    only when the user keeps the encoder resident
    (`unload_text_encoder_after_encode == False`), so letting the encoder's root
    decide would let two requests that share a key disagree about `is_distilled`
    on a pipeline the second one reuses. Nothing is lost in the shipped layout: a
    klein checkout keeps `model_index.json` in the root that is the parent of
    both `transformer/` and `vae/`.
    """
    return component_search_roots({key: paths.get(key) for key in ("transformer_path", "vae_path")})


def checkpoint_is_distilled(paths: dict[str, Any]) -> bool | None:
    """What the checkpoint declares about guidance distillation, as a TRI-STATE.

    `paths` is a normalized request or any mapping carrying `transformer_path`
    and `vae_path`. Returns the declared boolean, or `None` for "the checkpoint
    says nothing": no `model_index.json` beside the transformer or the VAE, one
    that cannot be read as a JSON object, or one carrying no boolean
    `is_distilled`. The first manifest that exists answers — a second one further
    up the search order is not consulted, exactly as in `discover_component_dir`.

    `None` is the normal answer for a hand-assembled component folder and must
    never be read as a refusal or as `False` in disguise; the caller decides what
    an unknown checkpoint gets and is expected to say so in the log.

    Never raises: an unreadable or malformed manifest is logged and answered
    `None`, because the only thing the flag can do is make a run CHEAPER, and
    failing a generation over a missing metadata file would be absurd.
    """
    for root in model_index_search_roots(paths):
        candidate = root / _MODEL_INDEX_MARKER
        try:
            if not candidate.is_file():
                continue
        except OSError as exc:
            log.info("FLUX.2 klein: не удалось проверить %s (%s)", candidate, exc)
            continue
        data = _read_json_object(candidate)
        if data is None:
            log.info(
                "FLUX.2 klein: %s не читается как JSON-объект; is_distilled считается "
                "необъявленным.",
                candidate,
            )
            return None
        value = data.get("is_distilled")
        if isinstance(value, bool):
            return value
        if value is not None:
            log.info(
                "FLUX.2 klein: %s объявляет is_distilled значением %r, а не булевым; флаг "
                "считается необъявленным.",
                candidate,
                value,
            )
        return None
    return None


# =====================================================================
#  VAE tiling threshold
# =====================================================================
def vae_tile_threshold_pixels(vae_path: str) -> int:
    """Output-pixel side ABOVE which the VAE's tiled decode path engages.

    Enabling tiling is not the same as tiling: `enable_tiling()` only flips
    `use_tiling`, and `AutoencoderKLFlux2._decode` takes the tiled branch only
    when a LATENT side exceeds `tile_latent_min_size`
    (`diffusers/models/autoencoders/autoencoder_kl_flux2.py`). Both thresholds
    come from the VAE's own config: `tile_sample_min_size = sample_size` and
    `tile_latent_min_size = int(sample_size / 2 ** (len(block_out_channels) - 1))`.
    Multiplying that latent threshold back by the same scale factor answers the
    question in the units every caller has — output pixels. On the shipped klein
    VAE (`sample_size: 1024`, four `block_out_channels`) the answer is 1024, so a
    region no side of which exceeds 1024 px is decoded UNTILED however the flag is
    set — which is most of them, since `params.MAX_REGION_PIXELS` caps the area at
    1 MP.

    `vae_path` is the user-supplied VAE path; a weights FILE is normalized to the
    folder holding its `config.json`, exactly as the loader does.

    Returns `VAE_TILE_THRESHOLD_FALLBACK_PIXELS` — a threshold no region this
    service accepts can reach, i.e. "assume tiling will NOT help" — when the
    config cannot be read or does not carry both fields, and logs the reason. The
    fallback deliberately rounds toward the EXPENSIVE answer: the only consumer is
    the memory forecast behind a guard whose failure mode is the OOM killer, so
    over-reserving costs a run that might have fitted while under-reserving costs
    the user's whole session. `components.text_encoder_resident_bytes` rounds up
    for the same reason.
    """
    try:
        config_dir = component_dir_for_path(Path(vae_path))
    except OSError as exc:
        log.info(
            "FLUX.2 klein: не удалось определить каталог VAE %s (%s); порог тайлинга VAE "
            "считается недостижимым (%d px).",
            vae_path,
            exc,
            VAE_TILE_THRESHOLD_FALLBACK_PIXELS,
        )
        return VAE_TILE_THRESHOLD_FALLBACK_PIXELS
    data = _read_json_object(config_dir / _MODEL_CONFIG_MARKER)
    sample_size = data.get("sample_size") if data is not None else None
    # diffusers itself accepts a sequence here and takes its first entry.
    if isinstance(sample_size, (list, tuple)) and sample_size:
        sample_size = sample_size[0]
    blocks = data.get("block_out_channels") if data is not None else None
    if (
        isinstance(sample_size, bool)
        or not isinstance(sample_size, int)
        or sample_size <= 0
        or not isinstance(blocks, (list, tuple))
        or not blocks
    ):
        log.info(
            "FLUX.2 klein: %s не объявляет sample_size и block_out_channels — порог тайлинга "
            "VAE считается недостижимым (%d px), декод прогнозируется как нетайловый.",
            config_dir / _MODEL_CONFIG_MARKER,
            VAE_TILE_THRESHOLD_FALLBACK_PIXELS,
        )
        return VAE_TILE_THRESHOLD_FALLBACK_PIXELS
    scale = 2 ** (len(blocks) - 1)
    # The truncation is diffusers': `int(sample_size / scale)`. Multiplying it
    # back is what makes this answer the PIXEL side of the very latent side the
    # gate compares, and not a second, subtly different number.
    latent_threshold = int(sample_size / scale)
    if latent_threshold <= 0:
        log.info(
            "FLUX.2 klein: sample_size %d и %d блоков VAE дают нулевой латентный порог тайлинга; "
            "порог считается недостижимым (%d px).",
            sample_size,
            len(blocks),
            VAE_TILE_THRESHOLD_FALLBACK_PIXELS,
        )
        return VAE_TILE_THRESHOLD_FALLBACK_PIXELS
    return latent_threshold * scale

# =====================================================================
#  Text-encoder truncation and its resident size
# =====================================================================
def _encoder_layer_index(tensor_name: str) -> int | None:
    """Decoder-layer index a Qwen3 checkpoint tensor belongs to, or `None`.

    `None` means the tensor is not part of a numbered decoder layer at all
    (`model.embed_tokens.weight`, `model.norm.weight`, `lm_head.weight`), so no
    layer count can remove it.
    """
    if not tensor_name.startswith(_ENCODER_LAYER_PREFIX):
        return None
    rest = tensor_name[len(_ENCODER_LAYER_PREFIX):]
    head = rest.split(".", 1)[0]
    return int(head) if head.isdigit() else None


def _encoder_tensor_materialized(tensor_name: str, keep_layers: int) -> bool:
    """Whether `tensor_name` still ends up in memory after a truncated load.

    Two families do not: `lm_head.*`, which `Qwen3Model` does not define (and
    which the shipped klein checkpoints do not even store — they tie it to
    `embed_tokens`), and every decoder layer at or above `keep_layers`, which the
    truncated config never constructs. Everything else is materialized.
    """
    if tensor_name.startswith(_ENCODER_LM_HEAD_PREFIX):
        return False
    index = _encoder_layer_index(tensor_name)
    return index is None or index < keep_layers


def _safetensors_materialized_bytes(path: Path, keep_layers: int) -> int | None:
    """Bytes of `path`'s tensors that survive a truncated load, or `None`.

    `None` says the header could not be read as a safetensors one — a `.bin`
    checkpoint, a truncated download, a placeholder file — and the caller must
    then fall back to the file's full size rather than guess a smaller number:
    over-reporting a memory need refuses a run that might have fitted, while
    under-reporting one invites the OOM killer, and only the second is
    unrecoverable.
    """
    try:
        header = read_safetensors_header(path)
    except ValueError as exc:
        log.debug("FLUX.2 klein: заголовок %s не разобран (%s)", path, exc)
        return None
    total = 0
    for name, entry in header.items():
        if name == "__metadata__" or not isinstance(entry, dict):
            continue
        offsets = entry.get("data_offsets")
        if not (isinstance(offsets, list) and len(offsets) == 2):
            continue
        try:
            span = int(offsets[1]) - int(offsets[0])
        except (TypeError, ValueError):
            continue
        if span > 0 and _encoder_tensor_materialized(name, keep_layers):
            total += span
    return total


def text_encoder_resident_bytes(path: str, keep_layers: int = ENCODER_KEEP_LAYERS) -> int:
    """Bytes of the text encoder a run actually MATERIALIZES in host memory.

    Deliberately NOT `_weight_bytes`, and the two must not be merged. The pair
    has opposite jobs: `_weight_bytes` answers "how big is this on disk", which
    is what `status.components[*].size_bytes` shows the user, while this answers
    "how much of it will exist in RAM", which is what the memory forecast needs.
    Since the encoder is loaded as a TRUNCATED `Qwen3Model` those two figures
    genuinely differ — on the shipped klein 4B encoder the trimmed layers 28..35
    are 20.1% of the 7.492 GiB of tensors — and reporting the disk figure to the
    forecast over-reserved by that much on every phase that carries the encoder.

    Only safetensors shards can be inspected without reading a tensor
    (`read_safetensors_header`), so a shard whose header does not parse and any
    non-safetensors weight file counts in FULL; see
    `_safetensors_materialized_bytes` for why the fallback rounds up.

    `path` is walked exactly like `_weight_bytes` walks it — a file counts as
    itself, a directory as its weight files — so the resident figure can never
    exceed the disk figure for the same path.
    """
    source = Path(path)
    try:
        if source.is_file():
            candidates = [source]
        elif source.is_dir():
            candidates = [
                entry
                for entry in source.rglob("*")
                if entry.is_file() and entry.suffix.lower() in _WEIGHT_SUFFIXES
            ]
        else:
            return 0
        total = 0
        for entry in candidates:
            materialized = (
                _safetensors_materialized_bytes(entry, keep_layers)
                if entry.suffix.lower() == ".safetensors"
                else None
            )
            total += int(entry.stat().st_size) if materialized is None else materialized
        return total
    except OSError as exc:
        log.debug("FLUX.2 klein: could not size %s (%s)", path, exc)
        return 0


def text_encoder_truncation_kwargs(
    encoder_dir: Path, keep_layers: int = ENCODER_KEEP_LAYERS
) -> dict[str, Any]:
    """Loader kwargs that cut the encoder down to `keep_layers` decoder layers.

    Reads `encoder_dir/config.json` and decides, explicitly, per branch — this is
    a documented and logged choice in every case, never a silent fallback:

    - `num_hidden_layers` absent (or the config unreadable): nothing is
      truncated, `{}` is returned and the reason is logged. A config we cannot
      read is a config we must not reason about.
    - `num_hidden_layers < keep_layers`: **refused**, naming the file, the count
      and the minimum. Clamping would hand the pipeline a `model.norm`-ed tensor
      where a raw layer output belongs (`ENCODER_KEEP_LAYERS`), i.e. a silently
      wrong embedding.
    - `num_hidden_layers == keep_layers`: `{}` — there is nothing to cut.
    - `num_hidden_layers > keep_layers`: `num_hidden_layers=keep_layers`, plus
      `layer_types` sliced to the same length WHEN the config carries a list.
      Both are required together: transformers 4.57's `layer_type_validation`
      compares the two and raises *"num_hidden_layers (28) must be equal to the
      number of layer types (36)"* if only the count is overridden. A config with
      no `layer_types` gets no such kwarg — transformers builds the list itself
      from the count.

    Nothing here hardcodes a layer count: the 9B encoder's config is not on this
    machine, so every number is taken from the config actually being loaded.

    # Raises
    `ValueError` when the encoder has fewer layers than the pipeline reads.
    """
    config_path = encoder_dir / _MODEL_CONFIG_MARKER
    data = _read_json_object(config_path)
    total = _config_int(data, "num_hidden_layers")
    if total is None:
        log.info(
            "FLUX.2 klein: %s не объявляет num_hidden_layers — текстовый энкодер грузится "
            "целиком, без отсечения неиспользуемых слоёв.",
            config_path,
        )
        return {}
    if total < keep_layers:
        raise ValueError(
            f"Текстовый энкодер слишком мелкий для FLUX.2 klein: {config_path} объявляет "
            f"num_hidden_layers = {total}, а пайплайн читает скрытые состояния слоёв "
            f"{', '.join(str(index) for index in TEXT_ENCODER_OUT_LAYER_INDICES)}, то есть "
            f"требует минимум {keep_layers}. Возьмите текстовый энкодер из каталога модели "
            "FLUX.2 klein."
        )
    if total == keep_layers:
        return {}

    kwargs: dict[str, Any] = {"num_hidden_layers": keep_layers}
    layer_types = (data or {}).get("layer_types")
    if isinstance(layer_types, list):
        # Sliced, never rebuilt: the shipped config names each layer's attention
        # kind and inventing them would change the architecture. A list shorter
        # than `keep_layers` is a config that contradicts its own
        # `num_hidden_layers`; the slice is then short too and transformers
        # raises its own error naming both numbers, which is the right diagnosis.
        kwargs["layer_types"] = list(layer_types[:keep_layers])
    log.info(
        "FLUX.2 klein: текстовый энкодер урезан с %d до %d слоёв — пайплайн читает только "
        "скрытые состояния %s, остальные слои не создаются и не читаются с диска.",
        total,
        keep_layers,
        ", ".join(str(index) for index in TEXT_ENCODER_OUT_LAYER_INDICES),
    )
    return kwargs


def _weight_bytes(path: str) -> int:
    """On-disk size of a component: one file, or every weight file in a folder.

    A bf16/fp16 checkpoint stores two bytes per parameter, which is also what it
    occupies once loaded, so for the transformer and the VAE the file size
    doubles as the weight-memory estimate.

    **This is the DISK figure and must stay one.** It is what
    `status.components[*].size_bytes` reports and what the client renders as a
    file size. The text encoder is the one component whose resident size is
    smaller than its files — it is loaded truncated — and the memory forecast
    therefore asks `text_encoder_resident_bytes` instead. Do not "fix" this
    function to account for that: it would make the size on screen disagree with
    the size on disk.
    """
    source = Path(path)
    try:
        if source.is_file():
            return int(source.stat().st_size)
        if not source.is_dir():
            return 0
        total = 0
        for entry in source.rglob("*"):
            if entry.is_file() and entry.suffix.lower() in _WEIGHT_SUFFIXES:
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
