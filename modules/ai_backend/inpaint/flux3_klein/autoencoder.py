"""
File: modules/ai_backend/inpaint/flux3_klein/autoencoder.py

Purpose:
The FLUX.3 klein image autoencoder: an 8x KL VAE plus a 2x2 patchify (16x in
total) whose latents are `4 * embed_dim`-channel tokens with `(t, h, w, l)` ids,
normalized by a non-affine BatchNorm's running statistics. Module and parameter
names are the BFL checkpoint keys (`ae.safetensors`).

Main responsibilities:
- `AeConfig` - the architecture dimensions (defaults = the released AE);
- `AutoEncoder` with `encode` (image -> tokens, ids; latent mean only) and
  `decode` (tokens, ids -> image);
- `latent_ids`, `latent_shape`;
- `build_autoencoder_meta` + `ae_tensor_cast` - the meta model and the run-dtype
  cast the large-model spec loads the AE with;
- `load_autoencoder` - the standalone loader: meta build + streaming fill from
  `ae.safetensors` (parity script, tests).

Key declarations:
- `AeConfig`, `AutoEncoder`, `Encoder`, `Decoder`, `ResnetBlock`, `AttnBlock`
- `DOWNSAMPLING_RATIO`

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/autoencoder.py`
  (commit e53f4ea), Copyright 2026 Black Forest Labs, Inc., licensed under the
  Apache License, Version 2.0 (see `LICENSE` in this directory).
- Modified: dimensions come from `AeConfig` instead of module constants (the
  defaults are the reference values); `encode`/`decode` validate their inputs,
  cast them to the parameter dtype, run with autocast disabled, apply the
  BatchNorm statistics in float32 (`F.batch_norm` in eval semantics instead of
  calling the module, so a bf16 AE cannot mix dtypes inside the norm) and
  return float32; `latent_shape` takes the ratio; streaming loader
  (`load_autoencoder`, over `runtime.safetensors_stream`) instead of the
  reference `checkpoint.load_checkpoint`.
- Run dtype: float32 is the reference (`pipeline.py` loads the AE in fp32);
  bf16 is an option the VRAM script measures (`params.AeDtype`).
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import torch
import torch.nn.functional as F
from torch import Tensor, nn

from ...runtime.safetensors_stream import fill_module_from_safetensors

log = logging.getLogger(__name__)

#: GroupNorm group count of every norm in the AE (reference `normalize`).
NORM_GROUPS = 32

#: Run dtypes `load_autoencoder` accepts.
_RUN_DTYPES = (torch.float32, torch.bfloat16, torch.float16)

#: State keys of the BatchNorm statistics: never cast (kept float32 / int64).
_BN_PREFIX = "bn."


@dataclass(frozen=True)
class AeConfig:
    """Autoencoder dimensions; the defaults are the released FLUX.3 klein AE.

    `embed_dim` is the VAE latent width (tokens are `4 * embed_dim` wide after
    the 2x2 patchify), `ch` the base width, `ch_mult` the per-level width
    multipliers, `num_res_blocks` the encoder ResNet blocks per level (the
    decoder has one more), `bn_eps` the latent BatchNorm epsilon.

    # Raises
    `ValueError` (construction) for a non-positive dimension, an empty
    `ch_mult`, or a level width not divisible by `NORM_GROUPS`.
    """

    embed_dim: int = 32
    ch: int = 128
    ch_mult: tuple[int, ...] = (1, 2, 4, 4)
    num_res_blocks: int = 2
    bn_eps: float = 1e-4

    def __post_init__(self) -> None:
        if self.embed_dim <= 0 or self.ch <= 0 or self.num_res_blocks <= 0 or not self.bn_eps > 0:
            raise ValueError(f"AeConfig: размеры должны быть положительными: {self}")
        if not self.ch_mult or any(mult <= 0 for mult in self.ch_mult):
            raise ValueError(f"AeConfig: ch_mult должен быть непустым и положительным: {self.ch_mult}")
        widths = [self.ch * mult for mult in self.ch_mult]
        if any(width % NORM_GROUPS for width in widths):
            raise ValueError(f"AeConfig: ширины уровней {widths} должны делиться на {NORM_GROUPS} (GroupNorm)")

    @property
    def downsampling_ratio(self) -> int:
        """Pixels per latent token side: `2 ** (levels - 1)` from the VAE times 2 from the patchify."""
        return 2 ** (len(self.ch_mult) - 1) * 2

    @property
    def latent_channels(self) -> int:
        """Token width after the 2x2 patchify, `4 * embed_dim`."""
        return 4 * self.embed_dim


#: Pixels per latent token side of the released AE (16).
DOWNSAMPLING_RATIO = AeConfig().downsampling_ratio


def swish(x: Tensor) -> Tensor:
    """SiLU, written as in the reference."""
    return x * torch.sigmoid(x)


def normalize(channels: int) -> nn.GroupNorm:
    """The AE's GroupNorm (32 groups, eps 1e-6, affine)."""
    return nn.GroupNorm(num_groups=NORM_GROUPS, num_channels=channels, eps=1e-6, affine=True)


class Upsample(nn.Module):
    """Nearest 2x upsample followed by a 3x3 conv."""

    def __init__(self, channels: int) -> None:
        super().__init__()
        self.conv = nn.Conv2d(channels, channels, 3, padding=1)

    def forward(self, x: Tensor) -> Tensor:
        """`x [B, C, H, W]` -> `[B, C, 2H, 2W]`."""
        return self.conv(F.interpolate(x, scale_factor=2.0, mode="nearest"))


class Downsample(nn.Module):
    """Stride-2 3x3 conv with a right/bottom zero pad of one pixel."""

    def __init__(self, channels: int) -> None:
        super().__init__()
        self.conv = nn.Conv2d(channels, channels, 3, stride=2, padding=0)

    def forward(self, x: Tensor) -> Tensor:
        """`x [B, C, H, W]` -> `[B, C, H/2, W/2]` (H and W even)."""
        return self.conv(F.pad(x, (0, 1, 0, 1)))


class ResnetBlock(nn.Module):
    """Two GroupNorm-swish-conv stages plus a residual (1x1 `nin_shortcut` when the width changes)."""

    def __init__(self, in_channels: int, out_channels: int) -> None:
        super().__init__()
        self.in_channels, self.out_channels = in_channels, out_channels
        self.norm1 = normalize(in_channels)
        self.conv1 = nn.Conv2d(in_channels, out_channels, 3, padding=1)
        self.norm2 = normalize(out_channels)
        self.dropout = nn.Dropout(0.0)
        self.conv2 = nn.Conv2d(out_channels, out_channels, 3, padding=1)
        if in_channels != out_channels:
            self.nin_shortcut = nn.Conv2d(in_channels, out_channels, 1)

    def forward(self, x: Tensor) -> Tensor:
        """`x [B, in, H, W]` -> `[B, out, H, W]`."""
        h = self.conv1(swish(self.norm1(x)))
        h = self.conv2(swish(self.norm2(h)))
        if self.in_channels != self.out_channels:
            x = self.nin_shortcut(x)
        return x + h


class AttnBlock(nn.Module):
    """Single-head spatial self-attention over all pixels, with a residual."""

    def __init__(self, channels: int) -> None:
        super().__init__()
        self.norm = normalize(channels)
        self.q = nn.Conv2d(channels, channels, 1)
        self.k = nn.Conv2d(channels, channels, 1)
        self.v = nn.Conv2d(channels, channels, 1)
        self.proj_out = nn.Conv2d(channels, channels, 1)

    def forward(self, x: Tensor) -> Tensor:
        """`x [B, C, H, W]` -> same shape: `x + proj_out(attention(norm(x)))`."""
        h = self.norm(x)
        b, c, height, width = h.shape

        def flat(t: Tensor) -> Tensor:
            """`[B, C, H, W]` -> `[B, 1, H*W, C]` (one head)."""
            return t.reshape(b, c, height * width).permute(0, 2, 1).unsqueeze(1).contiguous()

        h = F.scaled_dot_product_attention(flat(self.q(h)), flat(self.k(h)), flat(self.v(h)))
        return x + self.proj_out(h.squeeze(1).permute(0, 2, 1).reshape(b, c, height, width))


class Encoder(nn.Module):
    """Image `[B, 3, H, W]` -> `[B, 2 * embed_dim, H/8, W/8]` (mean and log-variance)."""

    def __init__(self, config: AeConfig) -> None:
        super().__init__()
        self.levels = len(config.ch_mult)
        self.quant_conv = nn.Conv2d(2 * config.embed_dim, 2 * config.embed_dim, 1)
        self.conv_in = nn.Conv2d(3, config.ch, 3, padding=1)
        self.down = nn.ModuleList()
        block_in = config.ch
        for level, mult in enumerate(config.ch_mult):
            down = nn.Module()
            down.block = nn.ModuleList()
            # Empty, kept because the reference module tree has it (no keys).
            down.attn = nn.ModuleList()
            for _ in range(config.num_res_blocks):
                down.block.append(ResnetBlock(block_in, config.ch * mult))
                block_in = config.ch * mult
            if level != self.levels - 1:
                down.downsample = Downsample(block_in)
            self.down.append(down)
        self.mid = nn.Module()
        self.mid.block_1 = ResnetBlock(block_in, block_in)
        self.mid.attn_1 = AttnBlock(block_in)
        self.mid.block_2 = ResnetBlock(block_in, block_in)
        self.norm_out = normalize(block_in)
        self.conv_out = nn.Conv2d(block_in, 2 * config.embed_dim, 3, padding=1)

    def forward(self, x: Tensor) -> Tensor:
        """Image `x [B, 3, H, W]` -> moments `[B, 2 * embed_dim, H/8, W/8]`."""
        h = self.conv_in(x)
        for level, down in enumerate(self.down):
            for block in down.block:
                h = block(h)
            if level != self.levels - 1:
                h = down.downsample(h)
        h = self.mid.block_2(self.mid.attn_1(self.mid.block_1(h)))
        return self.quant_conv(self.conv_out(swish(self.norm_out(h))))


class Decoder(nn.Module):
    """Latent `[B, embed_dim, h, w]` -> image `[B, 3, 8h, 8w]`."""

    def __init__(self, config: AeConfig) -> None:
        super().__init__()
        self.levels = len(config.ch_mult)
        self.post_quant_conv = nn.Conv2d(config.embed_dim, config.embed_dim, 1)
        block_in = config.ch * config.ch_mult[-1]
        self.conv_in = nn.Conv2d(config.embed_dim, block_in, 3, padding=1)
        self.mid = nn.Module()
        self.mid.block_1 = ResnetBlock(block_in, block_in)
        self.mid.attn_1 = AttnBlock(block_in)
        self.mid.block_2 = ResnetBlock(block_in, block_in)
        self.up = nn.ModuleList()
        for level in reversed(range(self.levels)):
            up = nn.Module()
            up.block = nn.ModuleList()
            # Empty, kept because the reference module tree has it (no keys).
            up.attn = nn.ModuleList()
            for _ in range(config.num_res_blocks + 1):
                up.block.append(ResnetBlock(block_in, config.ch * config.ch_mult[level]))
                block_in = config.ch * config.ch_mult[level]
            if level != 0:
                up.upsample = Upsample(block_in)
            self.up.insert(0, up)
        self.norm_out = normalize(block_in)
        self.conv_out = nn.Conv2d(block_in, 3, 3, padding=1)

    def forward(self, z: Tensor) -> Tensor:
        """Latent `z [B, embed_dim, h, w]` -> image `[B, 3, 8h, 8w]`."""
        h = self.conv_in(self.post_quant_conv(z))
        h = self.mid.block_2(self.mid.attn_1(self.mid.block_1(h)))
        for level in reversed(range(self.levels)):
            for block in self.up[level].block:
                h = block(h)
            if level != 0:
                h = self.up[level].upsample(h)
        return self.conv_out(swish(self.norm_out(h)))


class AutoEncoder(nn.Module):
    """The FLUX.3 klein AE: `encoder`, `decoder` and the non-affine latent BatchNorm `bn`.

    `encode`/`decode` run under `torch.inference_mode()` with autocast disabled;
    the BatchNorm statistics are always applied in float32 (eval semantics,
    whatever `self.training` is).
    """

    def __init__(self, config: AeConfig | None = None) -> None:
        super().__init__()
        self.config = config if config is not None else AeConfig()
        self.encoder = Encoder(self.config)
        self.decoder = Decoder(self.config)
        self.bn_eps = self.config.bn_eps
        self.bn = nn.BatchNorm2d(self.config.latent_channels, eps=self.bn_eps, affine=False, track_running_stats=True)

    def _param_dtype(self) -> torch.dtype:
        """The dtype the conv weights are stored in (the run dtype)."""
        return self.encoder.conv_in.weight.dtype

    def _normalize_latent(self, z: Tensor) -> Tensor:
        """`(z - running_mean) / sqrt(running_var + eps)` in float32."""
        return F.batch_norm(
            z.float(), self.bn.running_mean.float(), self.bn.running_var.float(), training=False, eps=self.bn_eps
        )

    @torch.inference_mode()
    def encode(self, image: Tensor) -> tuple[Tensor, Tensor]:
        """`image [3, H, W]` in [-1, 1] -> `(tokens [h*w, 4*embed_dim] float32, ids [h*w, 4] int64)`.

        Uses the latent mean only (no sampling). `H` and `W` must be multiples
        of `config.downsampling_ratio`; `ids` are `(0, row, col, 0)` on the
        image's device.

        # Raises
        `ValueError` for a non-`[3, H, W]` input or a side off the grid.
        """
        if image.ndim != 3 or image.shape[0] != 3:
            raise ValueError(f"AutoEncoder.encode: ожидается изображение [3, H, W], а не {tuple(image.shape)}")
        latent_shape(int(image.shape[1]), int(image.shape[2]), self.config.downsampling_ratio)
        embed_dim, channels = self.config.embed_dim, self.config.latent_channels
        with torch.autocast(device_type=image.device.type, enabled=False):
            moments = self.encoder(image.unsqueeze(0).to(self._param_dtype()))
        mean, _ = moments.chunk(2, dim=1)
        h, w = mean.shape[2] // 2, mean.shape[3] // 2
        # 2x2 patchify: channel index = c * 4 + dy * 2 + dx (reference order).
        z = mean.reshape(1, embed_dim, h, 2, w, 2).permute(0, 1, 3, 5, 2, 4).reshape(1, channels, h, w)
        z = self._normalize_latent(z)
        return z[0].permute(1, 2, 0).reshape(h * w, channels), latent_ids(h, w, z.device)

    @torch.inference_mode()
    def decode(self, tokens: Tensor, ids: Tensor) -> Tensor:
        """`tokens [h*w, 4*embed_dim]` in `ids` row-major order -> image `[3, 16h, 16w]` float32 in about [-1, 1].

        The grid size is read from `ids` (`max row + 1`, `max col + 1`), as in
        the reference; the tokens must be in `latent_ids` order.

        # Raises
        `ValueError` when the token count or width does not match the grid.
        """
        if ids.ndim != 2 or ids.shape[1] != 4 or ids.shape[0] == 0:
            raise ValueError(f"AutoEncoder.decode: id должны иметь форму [N, 4], а не {tuple(ids.shape)}")
        h, w = int(ids[:, 1].max()) + 1, int(ids[:, 2].max()) + 1
        embed_dim, channels = self.config.embed_dim, self.config.latent_channels
        if tuple(tokens.shape) != (h * w, channels):
            raise ValueError(
                f"AutoEncoder.decode: токены имеют форму {tuple(tokens.shape)}, для сетки {h}x{w} ожидается {(h * w, channels)}"
            )
        z = tokens.float().reshape(h, w, channels).permute(2, 0, 1).unsqueeze(0)
        running_var = self.bn.running_var.float().view(1, -1, 1, 1)
        running_mean = self.bn.running_mean.float().view(1, -1, 1, 1)
        z = z * torch.sqrt(running_var + self.bn_eps) + running_mean
        # Inverse patchify (see `encode`).
        z = z.reshape(1, embed_dim, 2, 2, h, w).permute(0, 1, 4, 2, 5, 3).reshape(1, embed_dim, 2 * h, 2 * w)
        with torch.autocast(device_type=z.device.type, enabled=False):
            image = self.decoder(z.to(self._param_dtype()))
        return image[0].float()


def latent_ids(h: int, w: int, device: torch.device | str = "cpu") -> Tensor:
    """`[h*w, 4]` int64 ids `(0, row, col, 0)` in row-major order."""
    rows, cols = torch.meshgrid(torch.arange(h, device=device), torch.arange(w, device=device), indexing="ij")
    zeros = torch.zeros_like(rows)
    return torch.stack([zeros, rows, cols, zeros], dim=-1).reshape(h * w, 4)


def latent_shape(height: int, width: int, ratio: int = DOWNSAMPLING_RATIO) -> tuple[int, int]:
    """The latent grid `(h, w)` of a `height` x `width` image.

    # Raises
    `ValueError` when a side is not a positive multiple of `ratio`.
    """
    if height <= 0 or width <= 0 or height % ratio or width % ratio:
        raise ValueError(f"Ширина и высота ({width}x{height}) должны быть положительными и кратными {ratio}")
    return height // ratio, width // ratio


def build_autoencoder_meta(config: AeConfig | None = None) -> AutoEncoder:
    """The AE of `config` (default: the released one) on `meta`, for a filler."""
    with torch.device("meta"):
        return AutoEncoder(config)


def ae_tensor_cast(dtype: torch.dtype) -> Callable[[str, Tensor], Tensor]:
    """The run-dtype cast of the AE: floats to `dtype`, except the BatchNorm statistics and non-float tensors (kept as stored).

    Device-agnostic (the large-model loader runs it on `meta`).

    # Raises
    `ValueError` (when called to build) for a `dtype` other than float32 /
    bfloat16 / float16.
    """
    if dtype not in _RUN_DTYPES:
        raise ValueError(f"load_autoencoder: тип {dtype} не поддерживается (ожидается один из {_RUN_DTYPES})")

    def cast(state_key: str, tensor: Tensor) -> Tensor:
        """Run-dtype cast that keeps the BatchNorm statistics and integer tensors as stored."""
        if state_key.startswith(_BN_PREFIX) or not tensor.is_floating_point():
            return tensor
        return tensor.to(dtype)

    return cast


def load_autoencoder(
    path: Path, *, device: torch.device | str, dtype: torch.dtype, config: AeConfig | None = None
) -> AutoEncoder:
    """Build the AE on `meta` and fill it from the safetensors file `path`, one tensor at a time.

    Float tensors are cast to `dtype`, except the BatchNorm statistics
    (`bn.running_mean` / `bn.running_var` stay float32) and non-float tensors
    (`bn.num_batches_tracked` stays int64). The key set must match exactly.
    Returns the module in eval mode on `device`.

    # Raises
    `ValueError` for a `dtype` other than float32 / bfloat16 / float16, or from
    `fill_module_from_safetensors` (malformed file, key or shape mismatch).
    """
    cast = ae_tensor_cast(dtype)
    autoencoder = build_autoencoder_meta(config)
    report = fill_module_from_safetensors(
        autoencoder,
        [path],
        device=device,
        key_map=lambda key: key,
        cast=cast,
        owner="FLUX.3 klein AE",
        logger=log,
    )
    log.info("FLUX.3 klein AE: загружено %d тензоров (%d байт) из %s, тип %s", report.loaded, report.bytes_read, path, dtype)
    return autoencoder.eval()
