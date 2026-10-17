"""
File: modules/ai_backend/inpaint/flux3_download.py

Purpose:
Acquisition of the FLUX.3 Image Klein weight package from Hugging Face into
`ManhwaStudio_AI_Models/side_models/FLUX.3-image-klein/`, behind the IPC
methods `inpaint.flux3_klein.download.check` and `.start`. The package layout
is the repository's own (the layout `flux3_klein.config.resolve_model_files`
reads), so a finished download is a package root the region editor runs from.

The repository `black-forest-labs/flux-3-image-klein` is GATED (the FLUX
Kommunity License): the user accepts its conditions on Hugging Face and sends a
token. The download is PER FORMAT: the selected format's DiT plus the shared
files, never the other formats' DiTs:

    every format   ae.safetensors, text_encoder/  (~9.2 GB)
    bf16           model.safetensors, config.json, manifest.json  (~11.1 GB)
    fp8r / nvfp4   variants/<format>/  (~6.3 / ~5.3 GB)

Everything else of the repository (sample images, `assets/`, READMEs) is
never fetched: the manifest names prefixes, and `README.md`, `.gitattributes`
and `*.jpg` / `*.gguf` files are excluded even inside them
(`hf_gated_download.is_excluded`).

Main responsibilities:
- `resolve_format` - the wire `format` (absent = `fp8r`), a typed refusal otherwise;
- `manifest_entries` / `download_target` - the format's manifest over
  `hf_gated_download` (prefix plan, access states, per-repository token,
  publish gates, free space, resumable transfer with throttled progress);
- `check_access` / `download` - the two wire answers;
- `model_root` - the destination (mirrored by Rust `ms_config::flux3_klein_dir`;
  the two must not drift).

Notes:
The token is a REQUEST FIELD: never read from the environment, persisted,
logged or put into a message (`hf_gated_download` scrubs hub messages).
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

from ..runtime.paths import side_models_root
from . import hf_gated_download
from .flux3_klein.config import WeightFormat
from .hf_gated_download import (  # DownloadCanceled is re-exported for the handler
    DownloadCanceled,  # noqa: F401
    DownloadTarget,
    ManifestEntry,
    ProgressCb,
    RepoSpec,
)

#: The official, gated FLUX.3 klein repository (token + accepted conditions).
REPO = "black-forest-labs/flux-3-image-klein"

#: The directory under `side_models/` that receives the package.
DIR_NAME = "FLUX.3-image-klein"

#: The model name in logs and user messages of the shared machinery.
LABEL = "FLUX.3 klein"

#: The format a request without `format` downloads (the editor's default).
DEFAULT_FORMAT = WeightFormat.FP8R

#: The `download` refusal for an empty token.
NO_TOKEN_MESSAGE = (
    "Не указан токен Hugging Face. Репозиторий FLUX.3 klein закрыт условиями "
    "(лицензия FLUX Kommunity): примите их на странице модели и укажите токен."
)


def model_root() -> Path:
    """Directory that receives the package: `<side_models>/FLUX.3-image-klein` (Rust: `ms_config::flux3_klein_dir`)."""
    return side_models_root() / DIR_NAME


def resolve_format(value: Any) -> WeightFormat:
    """The weight format named by the wire field `format`; absent / `None` / `""` = `fp8r`.

    # Errors
    `ValueError` naming the accepted values for anything else: an unknown
    format is never downgraded to the default (that would fetch gigabytes the
    user did not ask for).
    """
    if value is None or value == "":
        return DEFAULT_FORMAT
    if isinstance(value, str):
        for variant in WeightFormat:
            if variant.value == value:
                return variant
    raise ValueError(f"Неизвестный формат весов FLUX.3 klein: {value!r}. Доступны: {[variant.value for variant in WeightFormat]}.")


def manifest_entries(weight_format: WeightFormat) -> tuple[ManifestEntry, ...]:
    """The prefix rules of one format's download: the shared AE and text encoder, plus that format's DiT folder (module table)."""
    shared = (
        ManifestEntry(REPO, "ae.safetensors", "ae.safetensors"),
        ManifestEntry(REPO, "text_encoder/", "text_encoder"),
    )
    if weight_format is WeightFormat.BF16:
        dit = (
            ManifestEntry(REPO, "model.safetensors", "model.safetensors"),
            ManifestEntry(REPO, "config.json", "config.json"),
            ManifestEntry(REPO, "manifest.json", "manifest.json"),
        )
    else:
        folder = f"variants/{weight_format.value}"
        dit = (ManifestEntry(REPO, f"{folder}/", folder),)
    return shared + dit


def download_target(weight_format: WeightFormat) -> DownloadTarget:
    """The shared machinery's view of one format's download (`model_root()` read at call time: the tests' seam)."""
    return DownloadTarget(
        label=LABEL,
        repos=(RepoSpec(REPO, gated=True),),
        manifest=manifest_entries(weight_format),
        root=model_root(),
        no_token_message=NO_TOKEN_MESSAGE,
    )


def check_access(hf_token: str, *, weight_format: Any = None) -> dict[str, Any]:
    """The `inpaint.flux3_klein.download.check` answer: `{format, repos, plan, plan_error}`.

    `weight_format` is the RAW wire `format`; the answer echoes the format it
    was computed for. An empty token is `no_token` with no network call;
    `plan` is NULLABLE (`hf_gated_download.check_access`).

    # Errors
    `ValueError` for an unknown format.
    """
    selected = resolve_format(weight_format)
    return hf_gated_download.check_access(hf_token, download_target(selected), echo={"format": selected.value})


def download(
    hf_token: str,
    *,
    weight_format: Any = None,
    progress_callback: ProgressCb | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> dict[str, Any]:
    """Fetch every missing file of one format's package. Returns `{root, downloaded_bytes, skipped_files}`.

    `root` is the package root the editor runs from (`model_root()`).
    Transfer semantics (retry, `Range` resume, size + sha256 gates, free
    space, cancel at every chunk and retry wait): `hf_gated_download.download`.

    # Errors
    `ValueError` for an unknown format or an empty token; `DownloadCanceled`
    when the caller canceled; `RuntimeError` for an inaccessible repository, a
    manifest prefix that resolved to nothing, insufficient free space or a
    transport failure.
    """
    selected = resolve_format(weight_format)
    target = download_target(selected)
    counts = hf_gated_download.download(hf_token, target, progress_callback=progress_callback, should_cancel=should_cancel)
    return {"root": str(target.root), **counts}
