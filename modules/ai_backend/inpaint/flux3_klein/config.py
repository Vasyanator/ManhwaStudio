"""
File: modules/ai_backend/inpaint/flux3_klein/config.py

Purpose:
The FLUX.3 Image Klein weight package as data: the reference constants, the
parsed and validated `config.json`, which weight format a DiT checkpoint header
carries, and where every file of a package lives on disk.

Main responsibilities:
- reference constants (`MAX_REFERENCES`, `TEXT_MAX_TOKENS`, `TEXT_PAD_MULTIPLE`,
  `TEXT_OUTPUT_LAYERS`, `FLOW_MAP_KEY`);
- `read_package_config` -> `PackageConfig` (`DitConfig`, `SamplerConfig`);
- `detect_weight_format` (header -> `WeightFormat`) and
  `require_declared_format` (config vs header);
- `resolve_model_files` -> `ModelFiles`, `variant_availability`,
  `package_ae_file` / `package_text_encoder_dir` (the variant-independent files);
- the text encoder's checkpoint layout: `text_encoder_shards` (the shard files
  of `model.safetensors.index.json`) and the checkpoint key -> state key maps of
  the two encoder modes (`text_mode_key_map`, `vl_mode_key_map`).

Key declarations:
- `WeightFormat`, `DitConfig`, `SamplerConfig`, `PackageConfig`, `ModelFiles`

Package layout (the BFL export, `flux-3-image-klein`):
- `<root>/model.safetensors` + `<root>/config.json` - the BF16 DiT;
- `<root>/variants/<fp8r|nvfp4>/{model.safetensors,config.json}` - quantized DiTs;
- `<root>/ae.safetensors` - the autoencoder;
- `<root>/text_encoder/` - the Qwen3-VL text encoder directory.

Notes:
- Torch-free and stdlib-only; filesystem access is limited to reading
  `config.json` / the encoder's shard index and `is_file`/`is_dir` checks (the
  large-model spec, `large_model.py`, sizes the components from here without
  importing torch).
- The constants mirror values of black-forest-labs/flux-3-image-klein
  (commit e53f4ea, Apache-2.0, see `LICENSE` in this directory):
  `text_encoder.py` (`MAX_LENGTH`, `PAD_MULTIPLE`, the output layers),
  `pipeline.py` (`MAX_REFERENCES`). The reference hard-codes the DiT dimensions;
  this module reads them from `config.json` instead and refuses what the port
  does not implement (a non-flow-map "base" model, other output modes).
"""

from __future__ import annotations

import json
import logging
import re
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from types import MappingProxyType
from typing import Any

log = logging.getLogger(__name__)

#: Most reference images one request may carry (reference `pipeline.py`).
MAX_REFERENCES = 10

#: Text-token cap of the reference encoder (`text_encoder.py` `MAX_LENGTH`). The
#: package `config.json` declares `max_tokens: 8192`; the reference code ignores
#: it and so does this port (logged at INFO by `read_package_config`).
TEXT_MAX_TOKENS = 4096

#: Encoder output length is padded to a multiple of this (`PAD_MULTIPLE`).
TEXT_PAD_MULTIPLE = 80

#: Hidden-state indices the DiT's text input concatenates (8 x 2560 = 20480).
TEXT_OUTPUT_LAYERS: tuple[int, ...] = (4, 8, 12, 16, 20, 24, 28, 32)

#: A DiT key that exists only in the distilled flow-map model; the CFG "base"
#: model lacks it.
FLOW_MAP_KEY = "ref_time_in.in_layer.weight"

#: The only `config.json` `format_version` this module understands.
SUPPORTED_FORMAT_VERSION = 1

#: The DiT input modes the port implements, with their `in_channels` keys.
_DIT_INPUT_MODES = frozenset({"image", "image_cond", "text"})

#: The only output mode list the port implements.
_DIT_OUT_MODES = ["image"]

_METADATA_KEY = "__metadata__"


class WeightFormat(Enum):
    """How the DiT weights are stored. The value is the `quantization` string of
    `config.json` and the `variants/` sub-directory name (`bf16` has neither)."""

    BF16 = "bf16"
    FP8R = "fp8r"
    NVFP4 = "nvfp4"


@dataclass(frozen=True)
class DitConfig:
    """DiT dimensions from the `dit` section of `config.json`.

    Invariants (checked by `from_json_dict`): `hidden % heads == 0`,
    `sum(axes_dim) == hidden // heads`, `flow_map` is true. `in_channels` maps
    each input mode (`image`, `image_cond`, `text`) to its channel count and is
    read-only.
    """

    hidden: int
    heads: int
    depth: int
    depth_single: int
    mlp_ratio: float
    axes_dim: tuple[int, ...]
    theta: float
    in_channels: Mapping[str, int]
    image_rope_scale: float
    flow_map: bool

    @property
    def head_dim(self) -> int:
        """Per-head width, `hidden // heads`."""
        return self.hidden // self.heads

    @classmethod
    def from_json_dict(cls, data: Mapping[str, Any]) -> DitConfig:
        """Build and validate a `DitConfig` from the parsed `dit` object.

        # Raises
        `ValueError` naming the field for a missing or mistyped field, a head
        layout that does not divide, rope axes that do not sum to the head
        width, unknown input modes, an output mode other than `image`, a
        `vec_in_dim`, or `flow_map` not true (the CFG "base" model is out of
        scope).
        """
        hidden = _positive_int(data, "hidden", "dit")
        heads = _positive_int(data, "heads", "dit")
        depth = _positive_int(data, "depth", "dit")
        depth_single = _positive_int(data, "depth_single", "dit")
        mlp_ratio = _positive_float(data, "mlp_ratio", "dit")
        theta = _positive_float(data, "theta", "dit")
        image_rope_scale = _positive_float(data, "image_rope_scale", "dit")

        raw_axes = data.get("axes_dim")
        if not isinstance(raw_axes, list) or not raw_axes or not all(_is_positive_int(v) for v in raw_axes):
            raise ValueError(f"config.json: dit.axes_dim должен быть непустым списком положительных целых, а не {raw_axes!r}")
        axes_dim = tuple(int(v) for v in raw_axes)

        raw_channels = data.get("in_channels")
        if not isinstance(raw_channels, dict) or not all(
            isinstance(k, str) and _is_positive_int(v) for k, v in raw_channels.items()
        ):
            raise ValueError(f"config.json: dit.in_channels должен быть объектом «режим: число каналов», а не {raw_channels!r}")
        if set(raw_channels) != _DIT_INPUT_MODES:
            raise ValueError(
                f"config.json: dit.in_channels содержит режимы {sorted(raw_channels)}, "
                f"поддерживаются ровно {sorted(_DIT_INPUT_MODES)}"
            )

        if hidden % heads != 0:
            raise ValueError(f"config.json: dit.hidden={hidden} не делится на dit.heads={heads}")
        if sum(axes_dim) != hidden // heads:
            raise ValueError(
                f"config.json: сумма dit.axes_dim={list(axes_dim)} ({sum(axes_dim)}) "
                f"не равна ширине головы hidden/heads={hidden // heads}"
            )
        out_modes = data.get("out_modes", _DIT_OUT_MODES)
        if out_modes != _DIT_OUT_MODES:
            raise ValueError(f"config.json: dit.out_modes={out_modes!r} не поддерживается, ожидается {_DIT_OUT_MODES}")
        if data.get("vec_in_dim") is not None:
            raise ValueError(f"config.json: dit.vec_in_dim={data.get('vec_in_dim')!r} не поддерживается (ожидается null)")
        flow_map = data.get("flow_map")
        if flow_map is not True:
            raise ValueError(
                f"config.json: dit.flow_map={flow_map!r}. Поддерживается только дистиллированная "
                "модель FLUX.3 klein (flow map); базовая модель с CFG не поддерживается."
            )
        return cls(
            hidden=hidden,
            heads=heads,
            depth=depth,
            depth_single=depth_single,
            mlp_ratio=mlp_ratio,
            axes_dim=axes_dim,
            theta=theta,
            in_channels=MappingProxyType({k: int(v) for k, v in raw_channels.items()}),
            image_rope_scale=image_rope_scale,
            flow_map=True,
        )


@dataclass(frozen=True)
class SamplerConfig:
    """Sampler defaults: Euler steps, the schedule shift `alpha`, and the default
    output size `(width, height)` used when a request names none and has no
    reference to take it from."""

    steps: int = 16
    alpha: float = 6.93
    default_size: tuple[int, int] = (1024, 1024)


@dataclass(frozen=True)
class PackageConfig:
    """A validated `config.json`.

    `text_layers` / `pad_multiple` equal the reference constants (checked);
    `declared_max_tokens` is what the file declares and is NOT used
    (`TEXT_MAX_TOKENS` is); `quantization` is the declared format string
    (`None` for bf16).
    """

    dit: DitConfig
    sampler: SamplerConfig
    text_layers: tuple[int, ...]
    pad_multiple: int
    declared_max_tokens: int | None
    quantization: str | None

    @property
    def declared_format(self) -> WeightFormat:
        """The `WeightFormat` `quantization` declares (already validated)."""
        return WeightFormat.BF16 if self.quantization is None else WeightFormat(self.quantization)


def _is_positive_int(value: Any) -> bool:
    """True for an `int` (not `bool`) greater than zero."""
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _positive_int(data: Mapping[str, Any], key: str, section: str) -> int:
    """`data[key]` as a positive int, or `ValueError` naming `section.key`."""
    value = data.get(key)
    if not _is_positive_int(value):
        raise ValueError(f"config.json: {section}.{key} должен быть положительным целым, а не {value!r}")
    return int(value)


def _positive_float(data: Mapping[str, Any], key: str, section: str) -> float:
    """`data[key]` as a positive float (ints accepted), or `ValueError` naming `section.key`."""
    value = data.get(key)
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not value > 0:
        raise ValueError(f"config.json: {section}.{key} должен быть положительным числом, а не {value!r}")
    return float(value)


def _section(data: Mapping[str, Any], key: str) -> Mapping[str, Any]:
    """The JSON object `data[key]`, or `ValueError` naming it."""
    value = data.get(key)
    if not isinstance(value, dict):
        raise ValueError(f"config.json: раздел «{key}» отсутствует или не является объектом")
    return value


def _parse_sampler(data: Mapping[str, Any]) -> SamplerConfig:
    """Validate the `sampler` section; a non-null `guidance` (CFG) is refused."""
    steps = _positive_int(data, "steps", "sampler")
    alpha = _positive_float(data, "alpha", "sampler")
    size = data.get("size")
    if not isinstance(size, list) or len(size) != 2 or not all(_is_positive_int(v) for v in size):
        raise ValueError(f"config.json: sampler.size должен быть парой положительных целых, а не {size!r}")
    if data.get("guidance") is not None:
        raise ValueError(
            f"config.json: sampler.guidance={data.get('guidance')!r}. Модель с CFG не поддерживается "
            "(ожидается null у дистиллированной модели)."
        )
    return SamplerConfig(steps=steps, alpha=alpha, default_size=(int(size[0]), int(size[1])))


def _parse_text_encoder(data: Mapping[str, Any]) -> tuple[tuple[int, ...], int, int | None]:
    """Validate the `text_encoder` section against the reference constants.

    Returns `(layers, pad_multiple, declared_max_tokens)`. The layers and the
    pad multiple must equal `TEXT_OUTPUT_LAYERS` / `TEXT_PAD_MULTIPLE`: the
    encoder port implements exactly the reference layout, so a different one is
    refused rather than half-applied.
    """
    layers = data.get("layers")
    if not isinstance(layers, list) or tuple(layers) != TEXT_OUTPUT_LAYERS:
        raise ValueError(
            f"config.json: text_encoder.layers={layers!r} не поддерживается, ожидается {list(TEXT_OUTPUT_LAYERS)}"
        )
    pad_multiple = data.get("pad_multiple")
    if pad_multiple != TEXT_PAD_MULTIPLE or isinstance(pad_multiple, bool):
        raise ValueError(
            f"config.json: text_encoder.pad_multiple={pad_multiple!r} не поддерживается, ожидается {TEXT_PAD_MULTIPLE}"
        )
    max_tokens = data.get("max_tokens")
    if max_tokens is not None and not _is_positive_int(max_tokens):
        raise ValueError(f"config.json: text_encoder.max_tokens должен быть положительным целым или null, а не {max_tokens!r}")
    return TEXT_OUTPUT_LAYERS, TEXT_PAD_MULTIPLE, max_tokens


def read_package_config(path: Path) -> PackageConfig:
    """Read and validate a FLUX.3 klein `config.json` at `path`.

    A declared `text_encoder.max_tokens` other than `TEXT_MAX_TOKENS` is kept
    in `declared_max_tokens` and logged at INFO as ignored.

    # Raises
    `FileNotFoundError` when `path` does not exist; `ValueError` when it cannot
    be read or parsed, its `format_version` is not `SUPPORTED_FORMAT_VERSION`,
    `quantization` is not null / a `WeightFormat` value other than `bf16`, or a
    section is invalid (see `DitConfig.from_json_dict`).
    """
    if not path.is_file():
        raise FileNotFoundError(f"Не найден config.json модели FLUX.3 klein: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise ValueError(f"Не удалось прочитать {path}: {exc}") from exc
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"{path} не является корректным JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise ValueError(f"{path}: корень config.json должен быть объектом")

    version = data.get("format_version")
    if version != SUPPORTED_FORMAT_VERSION or isinstance(version, bool):
        raise ValueError(
            f"{path}: format_version={version!r} не поддерживается (ожидается {SUPPORTED_FORMAT_VERSION})"
        )
    quantization = data.get("quantization")
    allowed = sorted(fmt.value for fmt in WeightFormat if fmt is not WeightFormat.BF16)
    if quantization is not None and quantization not in allowed:
        raise ValueError(f"{path}: quantization={quantization!r} не поддерживается (ожидается null или одно из {allowed})")

    try:
        dit = DitConfig.from_json_dict(_section(data, "dit"))
        sampler = _parse_sampler(_section(data, "sampler"))
        text_layers, pad_multiple, declared_max_tokens = _parse_text_encoder(_section(data, "text_encoder"))
    except ValueError as exc:
        raise ValueError(f"{path}: {exc}") from exc

    if declared_max_tokens is not None and declared_max_tokens != TEXT_MAX_TOKENS:
        log.info(
            "FLUX.3 klein: %s объявляет text_encoder.max_tokens=%d; используется предел эталонного кода %d",
            path,
            declared_max_tokens,
            TEXT_MAX_TOKENS,
        )
    return PackageConfig(
        dit=dit,
        sampler=sampler,
        text_layers=text_layers,
        pad_multiple=pad_multiple,
        declared_max_tokens=declared_max_tokens,
        quantization=quantization,
    )


def detect_weight_format(header: Mapping[str, Any], *, source: Path | None = None) -> WeightFormat:
    """Classify a DiT safetensors `header` (the parsed JSON, not the file).

    Rules, by key suffix and dtype token (norm weights end in `.scale`, which is
    NOT a quantization key):
    - any `.weight_scale_2` key, with packed `U8` weights -> `NVFP4`;
    - else a `.weight_scale` key with `F8_*` weights and no `U8` -> `FP8R`;
    - else no quantization key at all and no `F8_*` / `U8` tensor -> `BF16`;
    - anything else -> `ValueError`. `source` only names the file in it.
    """
    has_scale = has_scale_2 = has_input_scale = False
    f8_weights = u8_weights = 0
    other_low_bit: list[str] = []
    for key, entry in header.items():
        if key == _METADATA_KEY:
            continue
        dtype = str(entry.get("dtype", "")).upper() if isinstance(entry, dict) else ""
        if key.endswith(".weight_scale_2"):
            has_scale_2 = True
        elif key.endswith(".weight_scale"):
            has_scale = True
        elif key.endswith(".input_scale"):
            has_input_scale = True
        elif key.endswith(".weight") and dtype.startswith("F8"):
            f8_weights += 1
        elif key.endswith(".weight") and dtype == "U8":
            u8_weights += 1
        elif dtype.startswith("F8") or dtype == "U8":
            other_low_bit.append(key)

    if not other_low_bit:
        if has_scale_2 and has_scale and u8_weights and not f8_weights:
            return WeightFormat.NVFP4
        if has_scale and not has_scale_2 and not has_input_scale and f8_weights and not u8_weights:
            return WeightFormat.FP8R
        if not (has_scale or has_scale_2 or has_input_scale or f8_weights or u8_weights):
            return WeightFormat.BF16
    where = f" {source}" if source is not None else ""
    raise ValueError(
        f"Не удалось определить формат весов FLUX.3 klein{where}: weight_scale={has_scale}, "
        f"weight_scale_2={has_scale_2}, input_scale={has_input_scale}, весов F8={f8_weights}, "
        f"весов U8={u8_weights}, прочих 8-битных тензоров={len(other_low_bit)}"
        + (f" (первые: {', '.join(other_low_bit[:5])})" if other_low_bit else "")
        + ". Ожидается bf16, fp8r или nvfp4 из официального пакета."
    )


def require_declared_format(config: PackageConfig, detected: WeightFormat) -> None:
    """Refuse a checkpoint whose header format differs from what `config.json` declares.

    # Raises
    `ValueError` naming both formats.
    """
    declared = config.declared_format
    if declared is not detected:
        raise ValueError(
            f"config.json объявляет формат весов «{declared.value}», а файл модели содержит "
            f"«{detected.value}». Вероятно, config.json лежит не в той папке варианта."
        )


@dataclass(frozen=True)
class ModelFiles:
    """Resolved, existing files of one package variant."""

    dit_file: Path
    dit_config: Path
    ae_file: Path
    text_encoder_dir: Path


def package_ae_file(root: Path) -> Path:
    """Where the package under `root` keeps the autoencoder (shared by every variant; not checked)."""
    return root / "ae.safetensors"


def package_text_encoder_dir(root: Path) -> Path:
    """Where the package under `root` keeps the text encoder (shared by every variant; not checked)."""
    return root / "text_encoder"


def resolve_model_files(root: Path, variant: WeightFormat) -> ModelFiles:
    """Locate the files of `variant` under the package `root` (layout: module header).

    # Raises
    `FileNotFoundError` naming every missing file or directory.
    """
    dit_dir = root if variant is WeightFormat.BF16 else root / "variants" / variant.value
    files = ModelFiles(
        dit_file=dit_dir / "model.safetensors",
        dit_config=dit_dir / "config.json",
        ae_file=package_ae_file(root),
        text_encoder_dir=package_text_encoder_dir(root),
    )
    missing = [str(path) for path in (files.dit_file, files.dit_config, files.ae_file) if not path.is_file()]
    if not files.text_encoder_dir.is_dir():
        missing.append(str(files.text_encoder_dir))
    if missing:
        raise FileNotFoundError(
            f"FLUX.3 klein ({variant.value}): не найдены файлы модели: {', '.join(missing)}"
        )
    return files


def variant_availability(root: Path) -> dict[WeightFormat, str | None]:
    """For every `WeightFormat`: `None` when its files are present, else the reason they are not.

    Only checks presence (`resolve_model_files`); it does not read headers.
    """
    result: dict[WeightFormat, str | None] = {}
    for variant in WeightFormat:
        try:
            resolve_model_files(root, variant)
        except FileNotFoundError as exc:
            result[variant] = str(exc)
        else:
            result[variant] = None
    return result


# =====================================================================
#  Text encoder checkpoint layout
# =====================================================================
#: Owner prefix of the encoder layout's refusal texts.
_ENCODER_OWNER = "FLUX.3 klein (текстовый энкодер)"

_INDEX_FILE = "model.safetensors.index.json"
_CHECKPOINT_ROOT = "model."
_LANGUAGE_PREFIX = "language_model."
_VISUAL_PREFIX = "visual."
_LAYER_KEY = re.compile(r"^layers\.(\d+)\.")


def text_encoder_shards(directory: Path) -> list[Path]:
    """The distinct shard files named by `<directory>/model.safetensors.index.json`, in name order.

    # Raises
    `FileNotFoundError` for a missing index or shard; `ValueError` for an index
    that is not JSON or has no `weight_map` object.
    """
    index_path = directory / _INDEX_FILE
    try:
        index = json.loads(index_path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise FileNotFoundError(f"{_ENCODER_OWNER}: не найден {index_path}") from exc
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"{_ENCODER_OWNER}: не удалось прочитать {index_path}: {exc}") from exc
    weight_map = index.get("weight_map") if isinstance(index, dict) else None
    if not isinstance(weight_map, dict) or not weight_map:
        raise ValueError(f"{_ENCODER_OWNER}: в {index_path} нет непустого объекта weight_map")
    names = sorted({str(name) for name in weight_map.values()})
    files = [directory / name for name in names]
    missing = [str(path) for path in files if not path.is_file()]
    if missing:
        raise FileNotFoundError(f"{_ENCODER_OWNER}: не найдены файлы весов: {', '.join(missing)}")
    return files


def _language_key(rest: str, keep_layers: int) -> str | None:
    """`rest` (a key below `model.language_model.`) unchanged, or `None` for a
    decoder layer at or above `keep_layers`."""
    match = _LAYER_KEY.match(rest)
    if match is not None and int(match.group(1)) >= keep_layers:
        return None
    return rest


def text_mode_key_map(keep_layers: int) -> Callable[[str], str | None]:
    """Checkpoint key -> `Qwen3VLTextModel` state key for the TEXT mode.

    Strips `model.language_model.`; skips `model.visual.*` and decoder layers
    `>= keep_layers` (never read). Any other key is returned unchanged, so the
    fill refuses it as unexpected (e.g. an `lm_head` this export never had).
    """

    def key_map(key: str) -> str | None:
        if not key.startswith(_CHECKPOINT_ROOT):
            return key
        rest = key[len(_CHECKPOINT_ROOT) :]
        if rest.startswith(_VISUAL_PREFIX):
            return None
        if rest.startswith(_LANGUAGE_PREFIX):
            return _language_key(rest[len(_LANGUAGE_PREFIX) :], keep_layers)
        return key

    return key_map


def vl_mode_key_map(keep_layers: int) -> Callable[[str], str | None]:
    """Checkpoint key -> `Qwen3VLModel` state key for the VL_REFS mode.

    Strips `model.`; skips language decoder layers `>= keep_layers`. The vision
    tower is loaded whole. Keys outside `model.` are returned unchanged (and so
    refused as unexpected).
    """

    def key_map(key: str) -> str | None:
        if not key.startswith(_CHECKPOINT_ROOT):
            return key
        rest = key[len(_CHECKPOINT_ROOT) :]
        if rest.startswith(_LANGUAGE_PREFIX):
            kept = _language_key(rest[len(_LANGUAGE_PREFIX) :], keep_layers)
            return None if kept is None else _LANGUAGE_PREFIX + kept
        return rest

    return key_map
