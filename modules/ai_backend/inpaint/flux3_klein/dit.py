"""
File: modules/ai_backend/inpaint/flux3_klein/dit.py

Purpose:
The FLUX.3 klein image DiT: per-mode blocks (image, image_cond, text each over
its own tokens), then shared single-stream blocks over every sample's tokens,
and a final layer that returns the velocity of the image tokens.

Main responsibilities:
- the math helpers `timestep_embedding`, `rope`, `rope_embed`, `rmsnorm_rope`,
  `layernorm_modulation`, `packed_attention`;
- the modules `RMSNorm`, `QKNorm`, `MLPEmbedder`, `ModeModulation`,
  `LastLayer`, `Block` and the model `Flux3Dit`.

Key declarations:
- `Flux3Dit(config: DitConfig)`; `forward(packed: Packed, timesteps_ref) -> Tensor`
- `Packed` / `MODES` come from `packing.py` (shared with the sampler).

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/dit.py`
  (commit e53f4ea), Copyright 2026 Black Forest Labs, Inc., licensed under the
  Apache License, Version 2.0 (see `LICENSE` in this directory).
- Modified:
  - config-driven dimensions (`DitConfig`) instead of module constants;
    `rope_embed` takes `axes_dim` and `theta`;
  - default SDPA (`F.scaled_dot_product_attention`, backend chosen by torch)
    instead of the forced `CUDNN_ATTENTION`, which fails on ROCm gfx1201;
  - `rmsnorm_rope` disables autocast for the tensor's own device type instead
    of a hard-coded `"cuda"` (same effect on CUDA/HIP, no warning on CPU);
  - only the distilled flow-map model is built (`DitConfig.flow_map` is always
    true), so `timesteps_ref` is mandatory and its length is checked; a pack
    without image tokens is refused;
  - `Packed` and `MODES` moved to `packing.py`; the class is `Flux3Dit`.
- Kept exactly as the reference, because they change the output: the phantom
  token of an empty mode (projected, never attended), bf16 timesteps (the
  embedding is cast back to the timestep dtype), text padding tokens attended
  by the DiT, the per-sample `MODES` order in the single-stream blocks, and
  module/parameter names identical to the BFL checkpoint keys.
- Dtypes: the module runs in the dtype of its parameters; the sampler wraps the
  call in `torch.autocast(bf16)`. Quantized linears (`quant.py`) return bf16,
  so a model with them must run in bf16 or under autocast.
"""

from __future__ import annotations

import logging
import math
from collections.abc import Sequence

import torch
import torch.nn.functional as F
from torch import Tensor, nn

from .config import DitConfig
from .packing import MODES, Packed

log = logging.getLogger(__name__)

#: Width of the sinusoidal timestep embedding fed to the time MLPs.
TIMESTEP_DIM = 256

#: Modes with their own input embedder and per-mode blocks (text has `txt_in`).
_CONTENT_MODES = ("image", "image_cond")

#: Modulation keys in the checkpoint: the text mode is named `txt` there.
_MODULATION_KEYS = ("image", "image_cond", "txt")


def _modulation_key(mode: str) -> str:
    """The modulation `ModuleDict` key of `mode` (`text` -> `txt`)."""
    return "txt" if mode == "text" else mode


def timestep_embedding(t: Tensor, dim: int = TIMESTEP_DIM, max_period: int = 10000) -> Tensor:
    """Sinusoidal embedding `[cos, sin]` of `1000 * t` (`t` shape `[L]`), in `t`'s dtype."""
    t = 1000.0 * t
    half = dim // 2
    freqs = torch.exp(-math.log(max_period) * torch.arange(half, device=t.device, dtype=torch.float32) / half)
    args = t[:, None].float() * freqs[None]
    return torch.cat([torch.cos(args), torch.sin(args)], dim=-1).to(t)


def rope(pos: Tensor, dim: int, theta: float) -> Tensor:
    """Rotation matrices `[..., n, dim // 2, 2, 2]` (float32) of one rope axis, computed in float64."""
    scale = torch.arange(0, dim, 2, dtype=torch.float64, device=pos.device) / dim
    out = torch.einsum("...n,d->...nd", pos, 1.0 / (theta**scale))
    out = torch.stack([torch.cos(out), -torch.sin(out), torch.sin(out), torch.cos(out)], dim=-1)
    return out.reshape(*out.shape[:-1], 2, 2).float()


def rope_embed(ids: Tensor, axes_dim: Sequence[int], theta: float) -> Tensor:
    """ids `(B, L, n_axes)` -> rotation matrices `(B, 1, L, head_dim // 2, 2, 2)`, outside autocast."""
    with torch.autocast(device_type=ids.device.type, enabled=False):
        emb = torch.cat([rope(ids[..., i], axes_dim[i], theta) for i in range(len(axes_dim))], dim=-3)
    return emb.unsqueeze(1)


def rmsnorm_rope(x: Tensor, scale: Tensor, freqs: Tensor, eps: float = 1e-6) -> Tensor:
    """RMS-normalize `x (B, H, L, D)` by `scale [D]` and rotate it by `freqs`, in float32; returns `x.dtype`."""
    with torch.autocast(device_type=x.device.type, enabled=False):
        b, h, length, d = x.shape
        x_f = x.float()
        x_norm = x_f * x_f.pow(2).mean(-1, keepdim=True).add(eps).rsqrt() * scale.float()
        x_r, freqs = x_norm.reshape(b, h, length, d // 2, 1, 2), freqs.float()
        rotated = freqs[..., 0] * x_r[..., 0] + freqs[..., 1] * x_r[..., 1]
        return rotated.reshape(b, h, length, d).to(x.dtype)


def layernorm_modulation(x: Tensor, scale: Tensor, shift: Tensor) -> Tensor:
    """`(1 + scale) * LayerNorm(x) + shift` (no affine LayerNorm, eps 1e-6)."""
    return (1.0 + scale) * F.layer_norm(x, (x.shape[-1],), eps=1e-6) + shift


def packed_attention(q: Tensor, k: Tensor, v: Tensor, bounds: Sequence[tuple[int, int]]) -> Tensor:
    """Per-sample attention over `(1, H, L, D)`: each `(start, end)` span attends
    only to itself; tokens outside every span (a phantom token) stay zero.

    Returns `(L, H * D)` in `q`'s dtype. The SDPA backend is torch's default
    choice (the reference forced cuDNN, which does not run on gfx1201).
    """
    _, heads, length, dim = q.shape
    out = torch.zeros(length, heads * dim, device=q.device, dtype=q.dtype)
    for start, end in bounds:
        if start == end:
            continue
        chunk = F.scaled_dot_product_attention(q[:, :, start:end], k[:, :, start:end], v[:, :, start:end])
        out[start:end] = chunk[0].transpose(0, 1).reshape(end - start, heads * dim)
    return out


class RMSNorm(nn.Module):
    """Holder of an RMS-norm `scale` (applied by `rmsnorm_rope`)."""

    def __init__(self, dim: int) -> None:
        super().__init__()
        self.scale = nn.Parameter(torch.ones(dim))


class QKNorm(nn.Module):
    """Query/key RMS norm + rope."""

    def __init__(self, dim: int) -> None:
        super().__init__()
        self.query_norm = RMSNorm(dim)
        self.key_norm = RMSNorm(dim)

    def forward(self, q: Tensor, k: Tensor, freqs: Tensor) -> tuple[Tensor, Tensor]:
        """Normalized and rotated `(q, k)`."""
        return rmsnorm_rope(q, self.query_norm.scale, freqs), rmsnorm_rope(k, self.key_norm.scale, freqs)


class MLPEmbedder(nn.Module):
    """`out_layer(silu(in_layer(x)))`, bias-free."""

    def __init__(self, in_dim: int, hidden_dim: int) -> None:
        super().__init__()
        self.in_layer = nn.Linear(in_dim, hidden_dim, bias=False)
        self.out_layer = nn.Linear(hidden_dim, hidden_dim, bias=False)

    def forward(self, x: Tensor) -> Tensor:
        """Embed `x [..., in_dim]` -> `[..., hidden_dim]`."""
        return self.out_layer(F.silu(self.in_layer(x)))


class ModeModulation(nn.Module):
    """Per-token `(shift, scale, gate)` from a conditioning vector."""

    def __init__(self, dim: int) -> None:
        super().__init__()
        self.lin = nn.Linear(dim, 3 * dim, bias=False)

    def forward(self, vec: Tensor) -> tuple[Tensor, Tensor, Tensor]:
        """`vec [L, dim]` -> three `[L, dim]` tensors: shift, scale, gate."""
        shift, scale, gate = self.lin(F.silu(vec)).chunk(3, dim=-1)
        return shift, scale, gate


class LastLayer(nn.Module):
    """Modulated LayerNorm + projection to the output channels."""

    def __init__(self, hidden: int, out_channels: int) -> None:
        super().__init__()
        self.linear = nn.Linear(hidden, out_channels, bias=False)
        self.adaLN_modulation = nn.Sequential(nn.SiLU(), nn.Linear(hidden, 2 * hidden, bias=False))

    def forward(self, x: Tensor, vec: Tensor) -> Tensor:
        """`x [L, hidden]`, `vec [L, hidden]` -> `[L, out_channels]` (shift first, then scale)."""
        shift, scale = self.adaLN_modulation(vec).chunk(2, dim=-1)
        return self.linear(layernorm_modulation(x, scale, shift))


class Block(nn.Module):
    """One transformer block: modulated LayerNorm, parallel attention + SwiGLU
    MLP, gated residual. Used for the per-mode and the single-stream stages."""

    def __init__(self, hidden: int, heads: int, mlp_ratio: float) -> None:
        super().__init__()
        self.heads = heads
        mlp_hidden = int(hidden * mlp_ratio)
        self.q_proj = nn.Linear(hidden, hidden, bias=False)
        self.k_proj = nn.Linear(hidden, hidden, bias=False)
        self.v_proj = nn.Linear(hidden, hidden, bias=False)
        self.mlp_in = nn.Linear(hidden, mlp_hidden * 2, bias=False)
        self.attn_out = nn.Linear(hidden, hidden, bias=False)
        self.mlp_out = nn.Linear(mlp_hidden, hidden, bias=False)
        self.norm = QKNorm(hidden // heads)

    def _split_heads(self, t: Tensor) -> Tensor:
        """`[L, H * D]` -> `[1, H, L, D]`."""
        return t.reshape(1, t.shape[0], self.heads, -1).transpose(1, 2)

    def forward(
        self, x: Tensor, freqs: Tensor, bounds: Sequence[tuple[int, int]], mod: tuple[Tensor, Tensor, Tensor]
    ) -> Tensor:
        """`x [L, hidden]` -> `[L, hidden]`; `bounds` are the per-sample attention spans, `mod` the
        per-token `(shift, scale, gate)`."""
        shift, scale, gate = mod
        x_mod = layernorm_modulation(x, scale, shift)
        q, k = self.norm(self._split_heads(self.q_proj(x_mod)), self._split_heads(self.k_proj(x_mod)), freqs)
        v = self._split_heads(self.v_proj(x_mod))
        attn = packed_attention(q, k, v.to(q.dtype), bounds)
        gate_in, up = self.mlp_in(x_mod).chunk(2, dim=-1)
        return x + gate * (self.attn_out(attn) + self.mlp_out(F.silu(gate_in) * up))


class Flux3Dit(nn.Module):
    """The distilled (flow-map) FLUX.3 klein DiT, dimensions from `config`.

    Parameter names equal the BFL checkpoint keys, so a `state_dict` loads
    without renaming. Built on `meta` by the loader, then its linears may be
    swapped for quantized ones (`quant.swap_quantized_linears`).
    """

    def __init__(self, config: DitConfig) -> None:
        super().__init__()
        # Only plain scalars are kept from `config`: its `in_channels` is a
        # MappingProxyType, which would make the module refuse `copy.deepcopy`.
        self.depth = config.depth
        self.axes_dim = tuple(config.axes_dim)
        self.theta = config.theta
        hidden, heads, ratio = config.hidden, config.heads, config.mlp_ratio
        channels = config.in_channels
        self.emb_in = nn.ModuleDict({m: nn.Linear(channels[m], hidden, bias=False) for m in _CONTENT_MODES})
        self.txt_in = nn.Linear(channels["text"], hidden, bias=False)
        self.time_in = MLPEmbedder(TIMESTEP_DIM, hidden)
        self.ref_time_in = MLPEmbedder(TIMESTEP_DIM, hidden)
        self.early_stream_modulations = nn.ModuleDict({k: ModeModulation(hidden) for k in _MODULATION_KEYS})
        self.single_stream_modulations = nn.ModuleDict({k: ModeModulation(hidden) for k in _MODULATION_KEYS})
        self.content_mode_blocks = nn.ModuleDict(
            {m: nn.ModuleList([Block(hidden, heads, ratio) for _ in range(config.depth)]) for m in _CONTENT_MODES}
        )
        self.txt_mode_blocks = nn.ModuleList([Block(hidden, heads, ratio) for _ in range(config.depth)])
        self.single_blocks = nn.ModuleList([Block(hidden, heads, ratio) for _ in range(config.depth_single)])
        self.final_layer = nn.ModuleDict({"image": LastLayer(hidden, channels["image"])})

    def _freqs(self, ids: Tensor) -> Tensor:
        """Rope rotation matrices of one token sequence `ids [L, 4]`."""
        return rope_embed(ids[None], self.axes_dim, self.theta)

    def forward(self, packed: Packed, timesteps_ref: Tensor) -> Tensor:
        """Velocity prediction `[N_image, in_channels["image"]]` for the image
        tokens, in `packed.data["image"]` order.

        `timesteps_ref [num_samples]` is the time each sample jumps to (the flow
        map's target); it is cast to the timestep dtype like the reference.

        # Raises
        `ValueError` when `timesteps_ref` is `None` or its length is not
        `packed.num_samples`, or when the pack has no image token (there would
        be nothing to predict; the reference returned uninitialized memory).
        """
        if packed.data["image"].shape[0] == 0:
            raise ValueError("Flux3Dit: в пакете нет токенов изображения")
        if timesteps_ref is None:
            raise ValueError("Flux3Dit: timesteps_ref обязателен для дистиллированной модели (flow map)")
        num_samples = packed.num_samples
        if timesteps_ref.dim() != 1 or timesteps_ref.shape[0] != num_samples:
            raise ValueError(
                f"Flux3Dit: timesteps_ref формы {tuple(timesteps_ref.shape)}, ожидается [{num_samples}]"
            )
        data, ids, timesteps = dict(packed.data), dict(packed.ids), dict(packed.timesteps)
        for m in MODES:
            # An empty mode gets one phantom token so its projections run; no
            # span covers it, so it is never attended and never reaches the output.
            if data[m].shape[0] == 0:
                data[m] = data[m].new_zeros(1, data[m].shape[1])
                ids[m] = ids[m].new_zeros(1, ids[m].shape[1])
                timesteps[m] = timesteps[m].new_zeros(1)

        vec = {m: self.time_in(timestep_embedding(timesteps[m])) for m in MODES}
        ref = self.ref_time_in(timestep_embedding(timesteps_ref.to(timesteps["image"].dtype)))
        for m in MODES:
            parts = [ref[s].expand(b - a, -1) for s, (a, b) in enumerate(packed.mode_indices[m]) if b > a]
            if parts:
                vec[m] = vec[m] + torch.cat(parts)
        early = {m: self.early_stream_modulations[_modulation_key(m)](vec[m]) for m in MODES}
        single = {m: self.single_stream_modulations[_modulation_key(m)](vec[m]) for m in MODES}
        x = {m: (self.txt_in if m == "text" else self.emb_in[m])(data[m]) for m in MODES}
        freqs = {m: self._freqs(ids[m]) for m in MODES}
        for depth in range(self.depth):
            for m in _CONTENT_MODES:
                x[m] = self.content_mode_blocks[m][depth](x[m], freqs[m], packed.mode_indices[m], early[m])
            x["text"] = self.txt_mode_blocks[depth](x["text"], freqs["text"], packed.mode_indices["text"], early["text"])

        # Re-pack sample-major for the single-stream stage: within each sample the
        # tokens follow MODES order, and `placement` remembers where each run went.
        chunks: list[Tensor] = []
        id_chunks: list[Tensor] = []
        mods: tuple[list[Tensor], list[Tensor], list[Tensor]] = ([], [], [])
        bounds: list[tuple[int, int]] = []
        placement: list[tuple[str, int, int, int]] = []
        offset = 0
        for s in range(num_samples):
            start = offset
            for m in MODES:
                a, b = packed.mode_indices[m][s]
                if b > a:
                    chunks.append(x[m][a:b])
                    id_chunks.append(ids[m][a:b])
                    for i in range(3):
                        mods[i].append(single[m][i][a:b])
                    placement.append((m, a, b, offset))
                    offset += b - a
            bounds.append((start, offset))
        h = torch.cat(chunks)
        freqs_single = self._freqs(torch.cat(id_chunks))
        single_mod = (torch.cat(mods[0]), torch.cat(mods[1]), torch.cat(mods[2]))
        for block in self.single_blocks:
            h = block(h, freqs_single, bounds, single_mod)

        image = torch.empty_like(x["image"])
        for m, a, b, start in placement:
            if m == "image":
                image[a:b] = h[start : start + b - a]
        return self.final_layer["image"](image, vec["image"])
