"""
File: modules/ai_backend/runtime/large_models/inventory.py

Purpose:
What a component weighs, block by block, read from checkpoint HEADERS only
(safetensors or GGUF)
(no tensor byte is read): which file tensors belong to which residency block,
their device bytes after conversion, their file bytes for load-time
forecasts, and the host transient of reading one of them (`load_host_bytes`).

Main responsibilities:
- `BlockSpan` - one file tensor with its block, state keys and sizes;
- `ComponentInventory` - a component's blocks in residency order, non-block
  weights, totals, the largest tensor and block, the largest load transient
  (`load_host_bytes`), and whether any block's tensors are scattered in the
  file (`fragmented`);
- `build_inventory` (one component, given files) and `build_inventories`
  (every component of a spec for one weight format).

Notes:
- The container dispatch, header parsers and per-tensor validation are
  `runtime.tensor_container.read_container_layout`'s (safetensors or GGUF);
  this module only groups. A block-quantized GGUF tensor is a `U8` span
  (`TensorSpan.quant_type`) whose file bytes are its packed rows; its device
  bytes are those bytes unless `ComponentSpec.device_itemsize` says otherwise.
- Device bytes assume the spec's conversion preserves the element count of a
  file tensor (renames and splits do); a dtype change is declared through
  `ComponentSpec.device_itemsize`.
- Load transient (the loader's filler reads ONE file tensor into an anonymous
  host buffer, converts it on the CPU, then writes it to its place): per file
  tensor `file_bytes + convert_bytes`, where `convert_bytes` are the bytes of
  the conversion's outputs that do not alias the source (a same-dtype `.to`, a
  rename, a split into views cost nothing; a cast or a `contiguous` copy costs
  its output). Derived by running `ComponentSpec.convert` on `meta` tensors
  (the loader's own layout derivation), so it needs torch for a component
  with a `convert`; without torch every converted output is counted (an upper
  bound). Temporaries INSIDE a conversion (e.g. a dequantisation's scratch)
  are not visible from its outputs and are not counted.
- Stdlib only, apart from the lazy `torch` import of the `meta` conversion.
"""

from __future__ import annotations

import logging
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Any

from ..safetensors_stream import SAFETENSORS_DTYPES_WITH_F8, TensorSpan
from ..tensor_container import read_container_layout
from .spec import ComponentSpec, LargeModelSpec

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class BlockSpan:
    """One file tensor as the inventory sees it.

    `block` is `None` for non-block weights; `state_keys` the module state
    entries the tensor becomes (never empty here: dropped tensors are not
    inventoried); `device_bytes` its size on the device after conversion;
    `convert_bytes` the host bytes its conversion allocates beyond the read
    buffer while it is loaded (module notes).
    """

    block: str | None
    key: str
    span: TensorSpan
    file: Path
    state_keys: tuple[str, ...]
    device_bytes: int
    convert_bytes: int = 0

    @property
    def file_bytes(self) -> int:
        """Bytes this tensor occupies in its file."""
        return self.span.nbytes

    @property
    def load_host_bytes(self) -> int:
        """Host bytes alive at once while the loader reads and converts this tensor (read buffer + conversion outputs)."""
        return self.span.nbytes + self.convert_bytes


@dataclass(frozen=True)
class ComponentInventory:
    """A component's weights grouped for residency planning (all sizes in bytes).

    `blocks` maps block name -> its tensors, in the component's block order (the
    first `k` blocks are the ones a plan with `resident_blocks=k` keeps on the
    device); `non_block` are the weights that always travel with the component.
    `bytes_*` are device bytes, `file_bytes_total` file bytes. `fragmented` is
    true when some block's tensors are not one consecutive run of one file
    (per-tensor reads, seeks on an HDD). `load_host_bytes` is the largest
    `BlockSpan.load_host_bytes`: the host transient on top of the component's
    resident host bytes while any part of it is read from its files.
    """

    name: str
    files: tuple[Path, ...]
    blocks: Mapping[str, tuple[BlockSpan, ...]]
    non_block: tuple[BlockSpan, ...]
    bytes_total: int
    bytes_by_block: Mapping[str, int]
    non_block_bytes: int
    file_bytes_total: int
    fragmented: bool
    max_tensor_bytes: int
    load_host_bytes: int

    @property
    def block_names(self) -> tuple[str, ...]:
        """Block names in residency order."""
        return tuple(self.blocks)

    @property
    def block_count(self) -> int:
        """Number of residency blocks (0 for a monolithic component)."""
        return len(self.blocks)

    def resident_bytes(self, resident_blocks: int) -> int:
        """Device bytes when the first `resident_blocks` blocks and the non-block weights are on the device."""
        names = self.block_names[: max(0, resident_blocks)]
        return self.non_block_bytes + sum(self.bytes_by_block[name] for name in names)

    def remainder_bytes(self, resident_blocks: int) -> int:
        """Bytes of the blocks after the first `resident_blocks`."""
        return self.bytes_total - self.resident_bytes(resident_blocks)

    def max_block_bytes(self, start: int = 0) -> int:
        """The largest block from index `start` on; `0` when there is none."""
        names = self.block_names[max(0, start):]
        return max((self.bytes_by_block[name] for name in names), default=0)

    def file_bytes_for(self, device_bytes: int) -> int:
        """File bytes that hold `device_bytes` of this component, proportionally (load-time estimate)."""
        if self.bytes_total <= 0:
            return 0
        return (device_bytes * self.file_bytes_total) // self.bytes_total


def _meta_torch() -> Any:
    """`torch` for the `meta` conversion, `None` (logged at INFO) when it cannot be imported."""
    try:
        import torch
    except ImportError as exc:
        log.info("inventory: torch is not importable (%s); conversion outputs are counted in full in the load transient", exc)
        return None
    return torch


def _meta_converter(component: ComponentSpec) -> Callable[[TensorSpan], int] | None:
    """`span -> convert_bytes` for `component` (module notes); `None` = count every converted output in full.

    A component without `convert` loads every tensor as read (the identity:
    `0`). With `convert`, the conversion runs on a `meta` tensor of the span's
    shape and dtype, and every output that is neither the source nor a view of
    it is counted.
    """
    convert = component.convert
    if convert is None:
        return lambda span: 0
    torch = _meta_torch()
    if torch is None:
        return None

    def convert_bytes(span: TensorSpan) -> int:
        source = torch.empty(span.shape, dtype=getattr(torch, span.torch_dtype_name), device="meta")
        try:
            produced = list(convert(span.name, source))
        except (RuntimeError, TypeError, ValueError, NotImplementedError) as exc:
            raise ValueError(f"Компонент «{component.name}»: преобразование «{span.name}» не выполняется на meta ({exc})") from exc
        return sum(value.numel() * value.element_size() for _, value in produced if value is not source and value._base is not source)

    return convert_bytes


def build_inventory(component: ComponentSpec, files: Sequence[Path], *, dtypes: Mapping[str, tuple[str, int]] = SAFETENSORS_DTYPES_WITH_F8) -> ComponentInventory:
    """Inventory `component` from the headers of `files` (header reads only).

    `dtypes` is the header dtype table (8-bit floats accepted by default: sizing
    is not where a model refuses a format).

    # Raises
    `ValueError` for an unreadable or malformed header (from
    `tensor_container`), a state key produced by two file tensors, a file
    tensor whose state keys fall into different blocks, a conversion that
    fails on `meta`, or a component with no tensor at all.
    """
    if not files:
        raise ValueError(f"Компонент «{component.name}»: не указано ни одного файла весов")
    item_bytes = {token: size for token, (_, size) in dtypes.items()}
    converter = _meta_converter(component)
    by_block: dict[str, list[BlockSpan]] = {}
    non_block: list[BlockSpan] = []
    seen_state: dict[str, str] = {}
    fragmented = False
    for path in files:
        spans = read_container_layout(path, dtypes=dtypes).spans
        previous_block: object = object()
        closed_in_file: set[str] = set()
        for span in spans:
            state_keys = tuple(component.state_keys_of(span.name))
            if not state_keys:
                continue
            for state_key in state_keys:
                if state_key in seen_state:
                    raise ValueError(f"Компонент «{component.name}»: ключ состояния «{state_key}» получается из двух тензоров («{seen_state[state_key]}» и «{span.name}»)")
                seen_state[state_key] = span.name
            blocks = {component.block_of(state_key) for state_key in state_keys}
            if len(blocks) != 1:
                raise ValueError(f"Компонент «{component.name}»: тензор «{span.name}» попадает в разные блоки {sorted(str(b) for b in blocks)}")
            block = blocks.pop()
            file_item = item_bytes[span.dtype_token]
            device_item = component.device_itemsize(span.name, span.dtype_token)
            elements = span.nbytes // file_item
            device_bytes = elements * (device_item if device_item is not None else file_item)
            convert_bytes = device_bytes if converter is None else converter(span)
            entry = BlockSpan(block=block, key=span.name, span=span, file=path, state_keys=state_keys, device_bytes=device_bytes, convert_bytes=convert_bytes)
            if block is None:
                non_block.append(entry)
            else:
                # A block is contiguous when its tensors form one uninterrupted
                # run of this file's span order and it lives in one file only.
                if block != previous_block and (block in closed_in_file or (block in by_block and by_block[block][0].file != path)):
                    fragmented = True
                by_block.setdefault(block, []).append(entry)
            if previous_block != block and isinstance(previous_block, str):
                closed_in_file.add(previous_block)
            previous_block = block
    if not by_block and not non_block:
        raise ValueError(f"Компонент «{component.name}»: в файлах нет ни одного загружаемого тензора")

    ordered = component.ordered_blocks(list(by_block))
    blocks = {name: tuple(by_block[name]) for name in ordered}
    bytes_by_block = {name: sum(entry.device_bytes for entry in spans) for name, spans in blocks.items()}
    every = [entry for spans in blocks.values() for entry in spans] + non_block
    inventory = ComponentInventory(
        name=component.name,
        files=tuple(files),
        blocks=MappingProxyType(blocks),
        non_block=tuple(non_block),
        bytes_total=sum(entry.device_bytes for entry in every),
        bytes_by_block=MappingProxyType(bytes_by_block),
        non_block_bytes=sum(entry.device_bytes for entry in non_block),
        file_bytes_total=sum(entry.file_bytes for entry in every),
        fragmented=fragmented,
        max_tensor_bytes=max(entry.device_bytes for entry in every),
        load_host_bytes=max(entry.load_host_bytes for entry in every),
    )
    log.debug("inventory %s: %d blocks, %d non-block tensors, %d device bytes, fragmented=%s", component.name, inventory.block_count, len(non_block), inventory.bytes_total, fragmented)
    return inventory


def build_inventories(spec: LargeModelSpec, options: Any, weight_format: str) -> dict[str, ComponentInventory]:
    """Inventory every component of `spec` for one weight format (`component.files(options, format)`).

    # Raises
    `ValueError` as `build_inventory` does, naming the component.
    """
    return {component.name: build_inventory(component, component.files(options, weight_format)) for component in spec.components}
