"""
File: modules/ai_backend/inpaint/flux2_download.py

Purpose:
Acquisition of the FLUX.2 klein weights from Hugging Face into
`ManhwaStudio_AI_Models/side_models/<variant directory>/`, behind the two IPC
methods `inpaint.flux2_klein.download.check` and `.start`. The wire contract is
`dev-docs/flux2_model_download.md`; this module is its Python half.

One VARIANT is downloadable here, selected by the request's `variant` field
(`"9b"`, also the default when the field is absent):

    9b  black-forest-labs/FLUX.2-klein-9B  gated, needs a token, ~34.7 GB,
        has an uncensored text encoder.

`"4b"` is REFUSED (`RETIRED_VARIANTS`): the 4B package is downloaded by the
application itself from its pinned model catalog (Rust
`ms_sysprobe::ai_models::external_catalog`), so each variant has exactly one
download owner. A variant carries no manifest of its own: `manifest_entries`
derives the rules from `variant.repo`.

The generic plan / access check / transfer machinery is
`hf_gated_download.py`; this module maps the FLUX.2 wire fields (`variant`,
`uncensored`) onto a `DownloadTarget` and keeps the FLUX.2 answers' shape.

Model layout produced here (every path relative to `model_root(variant)`):
    transformer/                official `transformer/*`
    text_encoder/               official `text_encoder/*`             — EXACTLY ONE
    text_encoder_uncensored/    uncensored repo minus its GGUF quants — OF THESE TWO
    tokenizer/                  official `tokenizer/*` — ALWAYS the official one
    vae/                        official `vae/*`
    scheduler/scheduler_config.json, model_index.json, LICENSE.md

The `uncensored` toggle SELECTS the encoder; it never adds a second one, so one
run downloads ~34.7 GB whichever way it is set. Both encoder directories may
still end up on disk from two runs with different settings — that is left
untouched, never re-fetched and never cleaned up.

Main responsibilities:
- the variant table and the encoder toggle (`uncensored` SELECTS one encoder);
- the FLUX.2 manifest (prefix rules over the variant's repositories);
- `check_access()` / `download()`: the two wire answers, over
  `hf_gated_download.check_access` / `.download` (plan by prefixes, size + LFS
  sha256 publish gate, token routing, free-space guard, throttled
  two-level progress, resumable transfer).

Key structures:
- `ModelVariant` — one downloadable variant (repo, destination directory,
  uncensored encoder if any; every repository is gated).
- `RETIRED_VARIANTS` — wire names this module refuses, with the user-facing
  reason (`"4b"`).
- `DownloadCanceled` — re-exported from `hf_gated_download` (the handler's
  one cancellation type).

Key functions:
- `resolve_variant()` — wire string -> `ModelVariant`, a typed refusal otherwise
  (an unknown or a retired name).
- `require_uncensored_supported()` — the second typed refusal (a variant that
  has no uncensored encoder; the 9B table entry has one).
- `download_target()` — variant + toggle -> `DownloadTarget`.
- `resolve_plan()` — the pure plan builder (a listing in, a plan out).
- `check_access()` — the `.check` answer (`plan` is NULLABLE; see its docstring).
- `download()` — the `.start` answer.
- `model_root()` / `component_paths()`

Notes:
The Hugging Face token is a REQUEST FIELD. This module never reads it from the
environment, never persists it, never logs it and never interpolates it into an
error message; a message that must mention it says "токен" and nothing more.
Messages coming back from `huggingface_hub` are additionally scrubbed of the
token value before they leave this module.

Every repository this module touches is gated, so every request needs a token
and an empty one short-circuits to `no_token` (`hf_gated_download`). The token
is sent at all three network sites — the access probe, the listing fetch and
the file transfer.

`HfApi.model_info()` succeeds for the gated repositories WITHOUT a token — the
metadata of a `gated: auto` repo is public — so a returned listing proves
nothing about access. The gate is probed with `HfApi.auth_check()` instead,
which is the only call that answers "may this token download these files".
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping

from ..runtime.paths import side_models_root
from .hf_gated_download import (  # DownloadCanceled is re-exported for the handler
    DownloadCanceled,  # noqa: F401
    DownloadTarget,
    ListedFile,
    ManifestEntry,
    PlannedFile,
    ProgressCb,
    RepoSpec,
)
from . import hf_gated_download


# --- Repositories ---------------------------------------------------------
#: The official, gated (`gated: auto`, licence `other`) FLUX.2 klein 9B
#: repository. A token AND accepted conditions are both required.
OFFICIAL_REPO_9B = "black-forest-labs/FLUX.2-klein-9B"

#: The gated uncensored text encoder of the 9B variant. Only fetched while the
#: toggle is on.
UNCENSORED_REPO_9B = "ponpoke/flux2-klein-9b-uncensored-text-encoder"

# --- Destination subdirectories -------------------------------------------
TRANSFORMER_SUBDIR = "transformer"
TEXT_ENCODER_SUBDIR = "text_encoder"
TEXT_ENCODER_UNCENSORED_SUBDIR = "text_encoder_uncensored"
VAE_SUBDIR = "vae"

#: The model name in logs and user messages of the shared machinery.
LABEL = "FLUX.2 klein"

#: The `download` refusal for an empty token (every FLUX.2 repository is gated).
NO_TOKEN_MESSAGE = (
    "Не указан токен Hugging Face. Оба репозитория FLUX.2 klein закрыты "
    "условиями, поэтому загрузка без токена невозможна."
)


# =====================================================================
#  Model variants
# =====================================================================
@dataclass(frozen=True)
class ModelVariant:
    """One downloadable FLUX.2 klein model variant.

    A frozen data table rather than an enum on purpose: what distinguishes the
    variants is DATA — a repository id, a destination directory, whether an
    uncensored encoder exists — and an enum
    would need exactly this table beside it anyway.

    A variant deliberately carries no manifest of its own: the FLUX.2 klein
    repositories share one folder layout (`transformer/`, `text_encoder/`,
    `tokenizer/`, `vae/`, `scheduler/`, `model_index.json`, `LICENSE.md`), and
    `manifest_entries` derives the rules from `repo`.

    - `key` is the wire value of the `variant` request field.
    - `dir_name` is the directory under `side_models/` that receives everything.
    - `uncensored_repo` is `None` when no uncensored encoder exists for this
      variant; asking for one is then a refusal, never a silent downgrade.

    Every variant's repositories are gated (`hf_gated_download` serves gated
    repositories only).
    """

    key: str
    repo: str
    dir_name: str
    uncensored_repo: str | None

    @property
    def supports_uncensored(self) -> bool:
        """Whether an uncensored text encoder exists for this variant."""
        return self.uncensored_repo is not None


#: FLUX.2 klein 9B: gated, ~34.7 GB, with an uncensored encoder. The DEFAULT.
VARIANT_9B = ModelVariant(
    key="9b",
    repo=OFFICIAL_REPO_9B,
    dir_name="FLUX.2-klein-9B",
    uncensored_repo=UNCENSORED_REPO_9B,
)

#: Every variant the two `.download.*` methods accept, keyed by wire value.
VARIANTS: dict[str, ModelVariant] = {VARIANT_9B.key: VARIANT_9B}

#: Wire names that WERE downloadable here and are now refused, each with the
#: user-facing reason. FLUX.2 klein 4B is downloaded by the application's own
#: pinned model catalog (Rust), so a backend download would be a second owner of
#: the same folder. A retired name is refused, never mapped onto another
#: variant: that would fetch weights the user did not ask for.
RETIRED_VARIANTS: dict[str, str] = {
    "4b": (
        "FLUX.2 klein 4B скачивается самим приложением (каталог моделей), "
        "а не бэкендом. Обновите ManhwaStudio."
    ),
}

#: The variant a request that carries no `variant` field means. Pinned by the
#: wire contract so a client built before the field existed keeps working.
DEFAULT_VARIANT = VARIANT_9B


def resolve_variant(value: Any) -> ModelVariant:
    """The variant named by the wire field `variant`. Absent or empty means 9B.

    `value` is the raw request field: `None` (absent), `""` (a client that
    serialises its default as an empty string) or one of the `VARIANTS` keys,
    matched case-insensitively after stripping.

    # Errors
    Raises `ValueError` for a non-string, for a `RETIRED_VARIANTS` name (its
    reason as the message: `"4b"` is downloaded by the application, not here)
    and, naming the accepted values, for an unknown name. Neither is ever
    downgraded to the default: that would fetch tens of gigabytes the user did
    not ask for into a directory they did not name.
    """
    if value is None:
        return DEFAULT_VARIANT
    if not isinstance(value, str):
        raise ValueError("Поле «variant» должно быть строкой.")
    key = value.strip().lower()
    if not key:
        return DEFAULT_VARIANT
    retired = RETIRED_VARIANTS.get(key)
    if retired is not None:
        raise ValueError(retired)
    variant = VARIANTS.get(key)
    if variant is None:
        offered = ", ".join(f"«{name}»" for name in sorted(VARIANTS))
        raise ValueError(
            f"Неизвестный вариант модели FLUX.2 klein: «{value}». Доступны: {offered}."
        )
    return variant


def require_uncensored_supported(variant: ModelVariant, uncensored: bool) -> None:
    """Refuse the uncensored encoder for a variant that has none.

    # Errors
    Raises `ValueError`. There is deliberately no downgrade to the official
    encoder: the user asked for a different set of weights, and handing them
    the other ones under the same button is exactly the silent fallback this
    package forbids.
    """
    if not uncensored or variant.supports_uncensored:
        return
    raise ValueError(
        f"Расцензуренный энкодер для {variant.key.upper()} пока не поддерживается — "
        "такого репозитория не существует. Отключите «Расцензуренный энкодер» или "
        f"выберите вариант «{VARIANT_9B.key}»."
    )


def shared_manifest(variant: ModelVariant) -> tuple[ManifestEntry, ...]:
    """Everything the pipeline needs except the text encoder, for `variant`.

    Taken from the variant's official repository in BOTH toggle states. The
    `transformer/` FOLDER is the one we take, never the same weights as the
    repository-root single file (`flux-2-klein-9b.safetensors`): diffusers 0.39 loads a single file through
    `from_single_file`, which accepts `device_map` and silently discards it, so
    the layered offload this engine depends on would be lost — and the file
    would be a second, duplicate copy of a transformer already on disk.
    """
    return (
        ManifestEntry(variant.repo, "transformer/", TRANSFORMER_SUBDIR),
        # The tokenizer ALWAYS comes from the official repo, in both toggle
        # states: the uncensored repo ships only `tokenizer.json` +
        # `tokenizer_config.json`, without `merges.txt`, `vocab.json`,
        # `special_tokens_map.json` or `added_tokens.json`, and
        # `flux2_klein._require_component_dir` refuses a tokenizer directory that
        # fails its markers. 17 MB is not worth a second failure mode.
        ManifestEntry(variant.repo, "tokenizer/", "tokenizer"),
        ManifestEntry(variant.repo, "vae/", VAE_SUBDIR),
        ManifestEntry(
            variant.repo, "scheduler/scheduler_config.json", "scheduler/scheduler_config.json"
        ),
        ManifestEntry(variant.repo, "model_index.json", "model_index.json"),
        ManifestEntry(variant.repo, "LICENSE.md", "LICENSE.md"),
    )


def official_encoder_manifest(variant: ModelVariant) -> tuple[ManifestEntry, ...]:
    """The official text encoder — the plan's encoder while the toggle is OFF."""
    return (ManifestEntry(variant.repo, "text_encoder/", TEXT_ENCODER_SUBDIR),)


def uncensored_encoder_manifest(variant: ModelVariant) -> tuple[ManifestEntry, ...]:
    """The uncensored text encoder — the plan's encoder while the toggle is ON.

    Everything that repository holds except the globally excluded files,
    flattened into its own directory.

    # Errors
    Raises `ValueError` through `require_uncensored_supported` for a variant
    that has no uncensored repository.
    """
    require_uncensored_supported(variant, True)
    return (ManifestEntry(str(variant.uncensored_repo), "", TEXT_ENCODER_UNCENSORED_SUBDIR),)


def model_root(variant: ModelVariant) -> Path:
    """Directory that receives the whole model: `<side_models>/<variant dir>`.

    The side-model root comes from `runtime.paths.side_models_root()`, the
    backend's single owner of it — never from root `config` or a local join.

    `variant` is REQUIRED and has no default on purpose: a caller that forgot it
    would silently write one variant's weights into the other's directory.
    """
    return side_models_root() / variant.dir_name


def required_repos(variant: ModelVariant, uncensored: bool) -> tuple[str, ...]:
    """Repositories the current variant and toggle state actually need, in report order.

    Only these are checked and reported: the variant's official repository
    always, its uncensored encoder only while the toggle is on.

    # Errors
    Raises `ValueError` when the toggle is on for a variant that has no
    uncensored repository (`require_uncensored_supported`).
    """
    require_uncensored_supported(variant, uncensored)
    if uncensored:
        return (variant.repo, str(variant.uncensored_repo))
    return (variant.repo,)


def manifest_entries(variant: ModelVariant, uncensored: bool) -> tuple[ManifestEntry, ...]:
    """The manifest rules in force for this variant and toggle state.

    **The two encoder rules are EXCLUSIVE: exactly one of them is ever in a
    plan.** The toggle SELECTS the encoder, it never adds a second one, so a 9B
    plan is ~34.7 GB in both states rather than ~51 GB with the toggle on.
    Everything else — the transformer, the tokenizer, the VAE and the configs —
    comes from the official repository either way, which is why both
    repositories are still access-checked when the toggle is on
    (`required_repos`).

    Both encoder directories may nevertheless end up on disk, from two runs with
    different settings. That is fine and is left alone: the missing-only filter
    never re-fetches the one already there, and nothing here ever deletes it.

    # Errors
    Raises `ValueError` when the toggle is on for a variant that has no
    uncensored repository.
    """
    encoder = (
        uncensored_encoder_manifest(variant) if uncensored else official_encoder_manifest(variant)
    )
    return shared_manifest(variant) + encoder


def component_paths(variant: ModelVariant, uncensored: bool) -> dict[str, str]:
    """Absolute component directories a finished download leaves behind.

    The three keys are the engine settings the Rust side writes back, so a
    finished download leaves a configured engine. `text_encoder` follows the
    toggle: it names the uncensored directory while the toggle is on, which is
    the same rule that decides which encoder was downloaded.
    """
    root = model_root(variant)
    encoder = TEXT_ENCODER_UNCENSORED_SUBDIR if uncensored else TEXT_ENCODER_SUBDIR
    return {
        "transformer": str(root / TRANSFORMER_SUBDIR),
        "text_encoder": str(root / encoder),
        "vae": str(root / VAE_SUBDIR),
    }


def download_target(variant: ModelVariant, uncensored: bool) -> DownloadTarget:
    """The shared machinery's view of one FLUX.2 request: repos, manifest, root.

    `model_root(variant)` is looked up at call time (the tests' seam).

    # Errors
    Raises `ValueError` for an uncensored request on a variant without one.
    """
    return DownloadTarget(
        label=LABEL,
        repos=tuple(RepoSpec(repo) for repo in required_repos(variant, uncensored)),
        manifest=manifest_entries(variant, uncensored),
        root=model_root(variant),
        no_token_message=NO_TOKEN_MESSAGE,
    )


def resolve_plan(
    listings: Mapping[str, Mapping[str, ListedFile | int]],
    *,
    variant: ModelVariant,
    uncensored: bool,
    root: Path | str | None = None,
) -> list[PlannedFile]:
    """Resolve this variant's manifest against live listings (`hf_gated_download.resolve_plan`).

    `root` defaults to `model_root(variant)`.

    # Errors
    `RuntimeError` for a missing listing or a prefix that resolved to nothing;
    `ValueError` for an uncensored request on a variant without one.
    """
    base = Path(root) if root is not None else model_root(variant)
    return hf_gated_download.resolve_plan(listings, manifest_entries(variant, uncensored), base)


def check_access(hf_token: str, *, uncensored: bool, variant: Any = None) -> dict[str, Any]:
    """The `inpaint.flux2_klein.download.check` answer: `{variant, repos, plan, plan_error}`.

    `variant` is the RAW wire field (absent/`None`/`""` means `"9b"`); the
    answer echoes the key it was computed for. An empty token is `no_token`
    without a network call, because every repository is gated. `plan` is
    NULLABLE (see `hf_gated_download.check_access`).

    # Errors
    Raises `ValueError` for an unknown or retired `variant` (`"4b"`) and for
    `uncensored` on a variant without one: malformed requests, not states of
    the world.
    """
    selected = resolve_variant(variant)
    require_uncensored_supported(selected, uncensored)
    return hf_gated_download.check_access(
        hf_token, download_target(selected, uncensored), echo={"variant": selected.key}
    )


def download(
    hf_token: str,
    *,
    uncensored: bool,
    variant: Any = None,
    progress_callback: ProgressCb | None = None,
    should_cancel: Callable[[], bool] | None = None,
) -> dict[str, Any]:
    """Fetch every missing file of the plan. Returns the `.start` answer.

    The answer is `{"paths": {...}, "downloaded_bytes": N, "skipped_files": N}`;
    `paths` are the three component directories the Rust side writes into the
    engine's settings, so a finished download leaves a configured engine.
    `variant` is the RAW wire field and selects both the repositories and the
    destination directory. Transfer semantics: `hf_gated_download.download`.

    # Errors
    Raises `ValueError` for an unknown or retired variant (`"4b"`), for an
    uncensored request on a variant that has no uncensored encoder, and for an
    empty token; `DownloadCanceled` when the caller canceled; `RuntimeError` for an
    inaccessible repository, a manifest prefix that resolved to nothing,
    insufficient free space, or a transport failure.
    """
    selected = resolve_variant(variant)
    require_uncensored_supported(selected, uncensored)
    counts = hf_gated_download.download(
        hf_token,
        download_target(selected, uncensored),
        progress_callback=progress_callback,
        should_cancel=should_cancel,
    )
    return {"paths": component_paths(selected, uncensored), **counts}
