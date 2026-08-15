"""
File: modules/ai_backend/watermark/code_fetch.py

Purpose:
Runtime fetch and import of the three upstream watermark-removal networks
(SLBR, WDNet, SplitNet). None of the three repositories carries a LICENSE file,
so their source is NEVER vendored into ManhwaStudio: it is downloaded onto the
user's machine on first use, into
`ManhwaStudio_AI_Models/side_models/WatermarkRemoval/<model>/src/`, verified by
SHA-256, and imported without ever being modified.

Main responsibilities:
- own the pinned manifest (commit SHA + per-file size and SHA-256) of the ten
  files that make up the three closures;
- download them atomically (`.part` + `os.replace`) and reject any byte that
  does not match its pinned hash;
- import them without touching `sys.path` and without ever handing a path to the
  import system: every file's bytes are read ONCE, hashed, and executed from
  memory, under pinned-`__path__` package stubs for SLBR (`src.*`) and SplitNet
  (`scripts.*`) plus `types.ModuleType` stubs for the training-only imports;
- construct an untrained `torch.nn.Module` per model with the exact constructor
  arguments the released checkpoints were saved with.

Key functions:
- `code_root()`, `is_code_ready()`, `ensure_model_code()`
- `build_network()`, `input_size_multiple()`, `requires_square_input()`

Notes:
- The upstream `__init__.py` files are deliberately absent from the manifest.
  `src/__init__.py` and `src/utils/__init__.py` pull `progress.bar`,
  `tensorboardX`, `pytorch_ssim`, `pytorch_iou`, `scipy.misc.imread` and
  `skimage.measure.compare_psnr`, none of which exist in this project's
  environment. The pinned-`__path__` stubs replace them.
- A pinned `__path__` is not cosmetic: this repository's own root contains a
  directory named `src/` (the Rust tree), which implicit-namespace resolution
  would happily merge into the `src` package.
- Downloaded code is executed. The SHA-256 check is therefore a hard failure,
  not a warning, and the pinned commit SHAs must never be replaced by branch
  names. Verification and execution operate on the same in-memory bytes, so a
  file swapped between the two cannot slip through.
- The loader leaves `sys.modules` exactly as it found it: names as generic as
  `src`, `scripts`, `unet_parts`, `vgg` or `dataloader` are shadowed only for
  the duration of the import and every pre-existing entry is restored.
"""

from __future__ import annotations

import hashlib
import importlib.util
import logging
import os
import sys
import threading
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from ..runtime.paths import program_root

log = logging.getLogger(__name__)

# Progress callback: (phase, done, total, label). Only "download" is emitted here.
ProgressCb = Callable[[str, int, int, str], None]

#: Catalog order. `MODEL_IDS[0]` is the default model everywhere in this domain.
MODEL_IDS: tuple[str, ...] = ("slbr", "wdnet", "splitnet")

_RAW_URL = "https://raw.githubusercontent.com/{owner}/{repo}/{commit}/{path}"
_DOWNLOAD_TIMEOUT_S = 60


@dataclass(frozen=True)
class _RemoteFile:
    """One upstream source file, pinned by size and SHA-256.

    `path` is relative to the repository root and is reproduced verbatim under
    `code_root(model_id)`, because the files use package-absolute imports
    (`from src.networks.blocks import ...`) that a flat dump cannot satisfy.

    `module` is the `sys.modules` name the file is executed as. The loader never
    hands a path to the import system; it compiles and executes the exact bytes
    it hashed, which is what keeps verification and execution from diverging.
    """

    path: str
    size: int
    sha256: str
    module: str


@dataclass(frozen=True)
class _RepoSpec:
    """An upstream repository pinned to one commit, plus the files we fetch.

    `commit` is always a full 40-hex commit SHA: a branch name would silently
    change what the user executes.

    `files` is in DEPENDENCY ORDER and the loader executes it in that order, so
    every `from <sibling> import ...` inside a closure resolves out of
    `sys.modules` and never reopens a file.
    """

    owner: str
    repo: str
    commit: str
    files: tuple[_RemoteFile, ...]


# Verified manifest (dev-docs/watermark_removal_plan.md §3.7). Sizes and hashes
# were cross-checked against the GitHub tree entries at the pinned commits.
# Each repo's tuple is in dependency order; see `_RepoSpec.files`.
_REPOS: dict[str, _RepoSpec] = {
    "slbr": _RepoSpec(
        owner="bcmi",
        repo="SLBR-Visible-Watermark-Removal",
        commit="47c665f1855ab6624cd52b28cefa797a9c8b96f7",
        files=(
            _RemoteFile(
                "src/utils/model_init.py",
                1752,
                "c3c7dd30573e1bfc0eafc93eeb2478c12b9144179db9317169454ce089abebcb",
                "src.utils.model_init",
            ),
            _RemoteFile(
                "src/networks/blocks.py",
                15124,
                "af28741b6da3f8d6330b571f324051ec33cae924aca32657669dcb84c9fcc384",
                "src.networks.blocks",
            ),
            _RemoteFile(
                "src/networks/resunet.py",
                14143,
                "ce41660178cea58ea372f713b817c6a3d0e55cf11c956d8d40119dfe96bba636",
                "src.networks.resunet",
            ),
        ),
    ),
    "wdnet": _RepoSpec(
        owner="MRUIL",
        repo="WDNet",
        commit="6788d33cde4432934b11fc951382fce82d892144",
        files=(
            # `WDNet.py` does an absolute `from unet_parts import *`, so
            # `unet_parts` must already be in `sys.modules` when it executes.
            _RemoteFile(
                "unet_parts.py",
                2444,
                "6aab7316bf0a0f413d2b58a76bd15484af5a526d2dd200d1a21cc169e9691010",
                "unet_parts",
            ),
            # Renamed on import: `WDNet` is a generic top-level name and this
            # module is only ever reached through `_loaded_modules`.
            _RemoteFile(
                "WDNet.py",
                12427,
                "d5f8fac2bf45b09c34a56f8c98900fac3f79622868ead4af272e1eb6a34bcd0f",
                "ms_watermark_wdnet_net",
            ),
        ),
    ),
    "splitnet": _RepoSpec(
        owner="vinthony",
        repo="deep-blind-watermark-removal",
        commit="72f0e61b9f06a60b2ffcd51b6efd26dfba73a1b5",
        files=(
            # Byte-identical to SLBR's src/utils/model_init.py (same upstream
            # template) - the shared hash is expected, not a copy-paste slip.
            _RemoteFile(
                "scripts/utils/model_init.py",
                1752,
                "c3c7dd30573e1bfc0eafc93eeb2478c12b9144179db9317169454ce089abebcb",
                "scripts.utils.model_init",
            ),
            _RemoteFile(
                "scripts/models/blocks.py",
                10365,
                "02e14fdb65c564950cbaa448725e45e21328f7fad54fa71d7433583cee1a5a36",
                "scripts.models.blocks",
            ),
            _RemoteFile(
                "scripts/models/rasc.py",
                5778,
                "e5d9b7b86bad8323963b71080cd56ae0f880a8547d36fced3e41a148f161f01e",
                "scripts.models.rasc",
            ),
            _RemoteFile(
                "scripts/models/unet.py",
                7038,
                "a89979fbe9f3d7e8eb239008a5820c7262ca3158812556f98c54044d46e13728",
                "scripts.models.unet",
            ),
            _RemoteFile(
                "scripts/models/sa_resunet.py",
                22050,
                "bb669313d6d88717b9d39fbe5afca124d9481e8aa5c8e2ccebb92feba38e6d3a",
                "scripts.models.sa_resunet",
            ),
        ),
    ),
}

#: Module (from the manifest) that defines each model's network class.
_NETWORK_MODULE: dict[str, str] = {
    "slbr": "src.networks.resunet",
    "wdnet": "ms_watermark_wdnet_net",
    "splitnet": "scripts.models.sa_resunet",
}

# Package directories that must exist as pinned-`__path__` stubs before the
# closure is imported, mapped model id -> ((package fullname, subdir), ...).
# The subdir is relative to `code_root(model_id)`.
_PACKAGE_STUBS: dict[str, tuple[tuple[str, str], ...]] = {
    "slbr": (
        ("src", "src"),
        ("src.networks", "src/networks"),
        ("src.utils", "src/utils"),
    ),
    "splitnet": (
        ("scripts", "scripts"),
        ("scripts.models", "scripts/models"),
        ("scripts.utils", "scripts/utils"),
    ),
    "wdnet": (),
}

# Training-only modules faked in `sys.modules` before the import, never patched
# into the downloaded source: model id -> ((module fullname, attribute), ...).
# Every attribute may be `None` - none is referenced on the construction or the
# forward path.
_MODULE_STUBS: dict[str, tuple[tuple[str, str], ...]] = {
    # WDNet.py:5,7,8 - the real modules read the CLWD dataset, require
    # tensorboardX (absent from this environment) and download ImageNet VGG16.
    "wdnet": (
        ("dataloader", "dataloader"),
        ("tensorboardX", "SummaryWriter"),
        ("vgg", "Vgg16"),
    ),
    # blocks.py:11 and rasc.py:11 - perceptual loss only.
    "splitnet": (("scripts.models.vgg", "Vgg16"),),
    "slbr": (),
}

# Top-level `sys.modules` names this loader owns while a model is imported. A
# name is owned when it equals a root or is a dotted child of one, which is what
# `_restore_owned_namespace` uses to put `sys.modules` back exactly as it found
# it once the import is done.
#
# Every one of these names is generic enough to collide with something else in a
# long-lived backend process (`src` in particular: this repository's own root is
# on `sys.path` once LaMa V2 has been used, and it contains the Rust `src/`
# tree). Nothing the closures need survives in `sys.modules` afterwards: their
# imports are all module-level, so the bound names live in the module globals we
# keep in `_loaded_modules`.
_OWNED_NAMESPACES: dict[str, tuple[str, ...]] = {
    "slbr": ("src",),
    "wdnet": ("unet_parts", "ms_watermark_wdnet_net", "dataloader", "tensorboardX", "vgg"),
    "splitnet": ("scripts",),
}

# Input-size contract, measured upstream across 128-384: SLBR and SplitNet fail
# with "Sizes of tensors must match except in dimension 1" unless every spatial
# size is a multiple of 16; WDNet accepts anything, including odd sizes.
_SIZE_MULTIPLE: dict[str, int] = {"slbr": 16, "wdnet": 1, "splitnet": 16}

# SLBR swaps H and W in three `F.interpolate(..., size=x.shape[2:][::-1])` calls
# (blocks.py:430,432; resunet.py:243), so it rejects non-square input while
# refinement is on. We pad to a square instead of editing someone else's file.
_REQUIRES_SQUARE: dict[str, bool] = {"slbr": True, "wdnet": False, "splitnet": False}

# Guards the download and the import; both are process-wide side effects.
_lock = threading.RLock()

# model id -> the imported module object that owns the network class.
_loaded_modules: dict[str, types.ModuleType] = {}


def _watermark_dir() -> Path:
    """Root of the on-disk layout: `side_models/WatermarkRemoval`.

    Prefers root `config.WATERMARK_DIR` and falls back to the program root
    (owned by `runtime/paths.py`) so this module stays importable in a bare test
    process where `config` is not on the path.
    """
    try:
        import config as _config

        configured = getattr(_config, "WATERMARK_DIR", None)
        if isinstance(configured, str) and configured:
            return Path(configured)
    except Exception:  # pragma: no cover - config is always importable in-app
        log.debug("watermark: root `config` unavailable, resolving via program_root()")
    return program_root() / "ManhwaStudio_AI_Models" / "side_models" / "WatermarkRemoval"


def model_dir(model_id: str) -> Path:
    """Per-model directory holding the checkpoint and the `src/` code checkout."""
    return _watermark_dir() / _validate_model_id(model_id)


def code_root(model_id: str) -> Path:
    """Checkout root of `model_id`'s downloaded network code.

    Upstream repository-relative paths are reproduced verbatim underneath, so
    SLBR's `src/networks/resunet.py` lands at
    `<code_root>/src/networks/resunet.py`. The doubled `src` segment is the
    upstream path meeting our directory name and is intentional: the closures
    use package-absolute imports and do not import from a flat dump.
    """
    return model_dir(model_id) / "src"


def manifest(model_id: str) -> tuple[tuple[str, int, str], ...]:
    """Pinned `(repo-relative path, size, sha256)` triples for `model_id`."""
    spec = _REPOS[_validate_model_id(model_id)]
    return tuple((f.path, f.size, f.sha256) for f in spec.files)


def pinned_commit(model_id: str) -> str:
    """Full commit SHA the code of `model_id` is pinned to."""
    return _REPOS[_validate_model_id(model_id)].commit


def input_size_multiple(model_id: str) -> int:
    """Spatial size divisor the network requires (16 for SLBR/SplitNet, 1 for WDNet)."""
    return _SIZE_MULTIPLE[_validate_model_id(model_id)]


def requires_square_input(model_id: str) -> bool:
    """Whether the network rejects non-square input (SLBR does; see the H/W swap)."""
    return _REQUIRES_SQUARE[_validate_model_id(model_id)]


def is_code_ready(model_id: str) -> bool:
    """Whether every manifest file of `model_id` is present AND hash-valid.

    Hashing ten files totalling ~90 KiB is cheap enough to do on every status
    query, and it is the only thing that distinguishes a complete checkout from
    a truncated or tampered one.

    This is a STATUS probe for `watermark.status` and for deciding whether to
    offer a download. It is deliberately NOT the gate the loader relies on: a
    result computed here says nothing about the bytes `_load_module` will
    execute later, so that function verifies its own read instead.
    """
    root = code_root(model_id)
    for entry in _REPOS[_validate_model_id(model_id)].files:
        dest = root / Path(entry.path)
        try:
            if not dest.is_file() or dest.stat().st_size != entry.size:
                return False
            if _sha256_file(dest) != entry.sha256:
                return False
        except OSError:
            return False
    return True


def ensure_model_code(model_id: str, progress_callback: ProgressCb | None = None) -> None:
    """Download the pinned network code of `model_id` if it is not already valid.

    Files that are present with the right size and hash are skipped, so a second
    call is a no-op plus ten hashes. Every download is atomic (`.part` +
    `os.replace`) and is verified before it is renamed into place.

    Progress is reported as `progress_callback("download", done, total, label)`
    in bytes, matching the FLUX two-phase contract.

    # Raises
    `ValueError` for an unknown `model_id`; `RuntimeError` when a download
    fails, when `requests` is missing, or when a downloaded file does not match
    its pinned size/SHA-256 - a hash mismatch is never tolerated, because these
    files are executed.
    """
    spec = _REPOS[_validate_model_id(model_id)]
    root = code_root(model_id)

    with _lock:
        missing = [f for f in spec.files if not _file_matches(root / Path(f.path), f)]
        if not missing:
            return

        total_bytes = sum(f.size for f in missing)
        done_bytes = 0

        def report(label: str) -> None:
            if progress_callback is None:
                return
            try:
                progress_callback("download", int(done_bytes), int(max(total_bytes, 1)), label)
            except Exception:
                # A broken progress sink must never abort a download.
                log.debug("watermark: progress callback raised", exc_info=True)

        log.info(
            "watermark: fetching %d source file(s) for %r from %s/%s@%s (%d bytes)",
            len(missing),
            model_id,
            spec.owner,
            spec.repo,
            spec.commit[:12],
            total_bytes,
        )
        report("Подготовка загрузки кода модели…")
        for entry in missing:
            url = _RAW_URL.format(
                owner=spec.owner, repo=spec.repo, commit=spec.commit, path=entry.path
            )
            dest = root / Path(entry.path)
            name = dest.name
            base = done_bytes

            def on_chunk(count: int, _base: int = base, _name: str = name) -> None:
                nonlocal done_bytes
                done_bytes = _base + count
                report(f"Скачивание {_name}")

            _download_verified(url, dest, entry, on_chunk)
            done_bytes = base + entry.size
            report(f"Скачано {name}")

        # A fresh checkout invalidates whatever we imported from the old one.
        _loaded_modules.pop(model_id, None)


def build_network(model_id: str) -> Any:
    """Construct the untrained network of `model_id` as a `torch.nn.Module`.

    The code must already be on disk (call `ensure_model_code` first). No
    weights are loaded and no device placement happens here; the caller owns
    both. The constructor arguments are the ones the released checkpoints were
    saved with and are load-bearing: SLBR's `k_center` defaults to 1 in
    `options.py` while the released checkpoint's own `scripts/test.sh` overrides
    it to 2, a difference of 43 459 parameters that fails `load_state_dict`.

    # Raises
    `ValueError` for an unknown `model_id`; `FileNotFoundError` when the code
    has not been downloaded; `RuntimeError` when a file on disk no longer
    matches its pinned SHA-256, or when the import or the construction fails.
    """
    module = _load_module(_validate_model_id(model_id))

    if model_id == "slbr":
        # All eight `args` fields the closure reads are set explicitly - see the
        # docstring. `lr` is only touched by `set_optimizers()`, which we never
        # call, but is cheap to provide.
        args = types.SimpleNamespace(
            use_refine=True,
            k_refine=3,
            k_skip_stage=3,
            mask_mode="res",
            k_center=2,
            bg_mode="res_mask",
            project_mode="simple",
            sim_metric="cos",
            lr=0.001,
        )
        return module.SLBR(args, shared_depth=1, blocks=3, long_skip=True)

    if model_id == "splitnet":
        return module.UnetVMS2AMv4(
            shared_depth=2,
            blocks=3,
            long_skip=True,
            use_vm_decoder=True,
            s2am="vms2am",
        )

    if model_id == "wdnet":
        # `generator(in_channels, out_channels)`; `class WDNet` in the same file
        # is the training driver and hardcodes `.cuda()` - never touch it.
        return module.generator(3, 3)

    raise ValueError(f"Неизвестная модель водяных знаков: {model_id!r}")


# =====================================================================
#  Import plumbing
# =====================================================================
def _load_module(model_id: str) -> types.ModuleType:
    """Import (once per process) the module that owns `model_id`'s network class.

    Everything happens under `_lock`, so concurrent first uses cannot race each
    other into a half-initialized `sys.modules` state, and so the shadowing of
    the owned namespace is never observed by another loader.

    The whole closure is read and verified BEFORE anything is registered, and
    the bytes that were hashed are the bytes that get compiled and executed —
    the import system is never handed a path, so a file swapped after
    verification cannot be executed. `sys.modules` is restored to its previous
    contents afterwards, including any entry this loader shadowed.

    # Raises
    `FileNotFoundError` when a manifest file is missing; `RuntimeError` when a
    file does not match its pinned size/SHA-256 or when execution fails.
    """
    with _lock:
        cached = _loaded_modules.get(model_id)
        if cached is not None:
            return cached

        root = code_root(model_id)
        spec = _REPOS[model_id]
        # Read and verify everything first: a failure here must not leave a
        # partially registered package behind.
        sources = [
            (entry, _read_verified_source(root / Path(entry.path), entry))
            for entry in spec.files
        ]

        saved = _snapshot_owned_namespace(model_id)
        try:
            # Packages first, so a namespaced stub such as `scripts.models.vgg`
            # can be bound onto a parent that already exists.
            for fullname, subdir in _PACKAGE_STUBS[model_id]:
                _make_pkg(fullname, root / subdir)
            _install_module_stubs(model_id)
            for entry, data in sources:
                _exec_source(entry.module, root / Path(entry.path), data)
            module = sys.modules[_NETWORK_MODULE[model_id]]
        finally:
            _restore_owned_namespace(model_id, saved)

        _loaded_modules[model_id] = module
        return module


def _snapshot_owned_namespace(model_id: str) -> dict[str, types.ModuleType]:
    """Copy every `sys.modules` entry this loader is about to own for `model_id`."""
    roots = _OWNED_NAMESPACES[model_id]
    return {name: module for name, module in sys.modules.items() if _is_owned(name, roots)}


def _restore_owned_namespace(model_id: str, saved: dict[str, types.ModuleType]) -> None:
    """Undo every `sys.modules` change inside `model_id`'s owned namespace.

    Names this loader added are removed and names it shadowed are put back, so a
    legitimate `src` / `scripts` / `unet_parts` elsewhere in the process is
    unaffected by a watermark model having been imported. Only the owned roots
    are touched, so genuine third-party imports the closure triggered (torch,
    torchvision, cv2, scipy) are left alone.
    """
    roots = _OWNED_NAMESPACES[model_id]
    for name in [name for name in sys.modules if _is_owned(name, roots)]:
        del sys.modules[name]
    sys.modules.update(saved)


def _is_owned(name: str, roots: tuple[str, ...]) -> bool:
    """Whether the module `name` is one of `roots` or a dotted child of one."""
    return any(name == root or name.startswith(root + ".") for root in roots)


def _make_pkg(fullname: str, path: Path) -> types.ModuleType:
    """Register a package stub in `sys.modules` whose `__path__` is exactly `path`.

    Always installs a fresh stub: an entry that happens to be there already is
    someone else's and is never reused (the caller has snapshotted it and will
    restore it). A single-entry `__path__` is what keeps this repository's own
    Rust `src/` directory (and anything else on `sys.path`) from being merged
    into the package as a namespace portion.

    # Raises
    `RuntimeError` when `fullname`'s parent package has not been created yet.
    """
    module = types.ModuleType(fullname)
    module.__path__ = [str(path)]  # type: ignore[attr-defined]
    module.__package__ = fullname
    parent, _, leaf = fullname.rpartition(".")
    if parent:
        parent_module = sys.modules.get(parent)
        if parent_module is None:
            raise RuntimeError(
                f"Внутренняя ошибка загрузки кода сети: пакет {parent!r} должен быть "
                f"создан раньше, чем {fullname!r}"
            )
        # Required: the import system binds a submodule onto its parent, and a
        # stub parent will not do that for us.
        setattr(parent_module, leaf, module)
    sys.modules[fullname] = module
    return module


def _install_module_stubs(model_id: str) -> set[str]:
    """Fake the training-only modules `model_id`'s closure imports at module level.

    A pre-existing module under one of these names is shadowed unconditionally
    rather than trusted: `vgg`, `dataloader` and `tensorboardX` are generic
    enough that a foreign module of that name would be executed by the closure's
    `from vgg import Vgg16`. The caller restores the shadowed entry afterwards.

    Returns the set of names installed, for logging and tests.
    """
    created: set[str] = set()
    for fullname, attribute in _MODULE_STUBS[model_id]:
        stub = types.ModuleType(fullname)
        # `None` is enough: neither attribute is referenced on the construction
        # or the forward path, only on the training/loss path we never enter.
        setattr(stub, attribute, None)
        parent, _, leaf = fullname.rpartition(".")
        if parent:
            parent_module = sys.modules.get(parent)
            if parent_module is not None:
                setattr(parent_module, leaf, stub)
        sys.modules[fullname] = stub
        created.add(fullname)
    return created


def _exec_source(module_name: str, path: Path, source: bytes) -> types.ModuleType:
    """Execute the verified `source` bytes as `module_name` and register it.

    `path` is used for the module's `__file__` and as the compile filename, so
    tracebacks still point at the file on disk — but the file is NOT reopened:
    only `source`, which the caller has already hashed, is executed.

    # Raises
    `RuntimeError` when the bytes do not compile or raise while executing.
    """
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None:
        raise RuntimeError(f"Не удалось подготовить импорт модуля: {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    try:
        code = compile(source, str(path), "exec", dont_inherit=True)
        exec(code, module.__dict__)  # noqa: S102 - hash-pinned upstream source
    except Exception as exc:
        sys.modules.pop(module_name, None)
        raise RuntimeError(
            f"Не удалось выполнить код сети.\nМодуль: {module_name}\nФайл: {path}\n"
            f"Ошибка: {exc}"
        ) from exc
    return module


def _read_verified_source(path: Path, entry: _RemoteFile) -> bytes:
    """Read `path` in full and return the bytes only if they match the manifest.

    This is the single point where a downloaded source file is trusted, and it
    is deliberately the same read whose result gets executed: hashing the file
    and then letting the import system reopen it would leave a window in which
    the executed bytes were never hashed.

    # Raises
    `FileNotFoundError` when the file is absent or unreadable; `RuntimeError`
    when its size or SHA-256 does not match the pinned manifest.
    """
    try:
        data = path.read_bytes()
    except OSError as exc:
        raise FileNotFoundError(
            "Код сети не загружен или недоступен. Скачайте код модели ещё раз.\n"
            f"Файл: {path}\nОшибка: {exc}"
        ) from exc

    actual = hashlib.sha256(data).hexdigest()
    if len(data) != entry.size or actual != entry.sha256:
        log.error(
            "watermark: integrity check failed for %s (expected %d bytes / %s, got %d bytes / %s)",
            path,
            entry.size,
            entry.sha256,
            len(data),
            actual,
        )
        raise RuntimeError(
            "Проверка целостности кода сети не пройдена — файл не будет выполнен.\n"
            f"Файл: {path}\n"
            f"Ожидалось: {entry.size} байт, SHA-256 {entry.sha256}\n"
            f"Получено:  {len(data)} байт, SHA-256 {actual}"
        )
    return data


# =====================================================================
#  Download plumbing
# =====================================================================
def _download_verified(
    url: str,
    dest: Path,
    entry: _RemoteFile,
    on_chunk: Callable[[int], None],
) -> None:
    """Stream `url` into `dest` atomically, verifying size and SHA-256 first.

    The bytes are written to `dest.part` and hashed while streaming; the file is
    renamed into place only after both the length and the digest match the
    manifest. A failed verification removes the partial file and raises.

    # Raises
    `RuntimeError` on a transport error, a missing `requests`, or a size/digest
    mismatch.
    """
    try:
        import requests
    except Exception as exc:  # pragma: no cover - requests is in requirements.txt
        raise RuntimeError(
            "Для загрузки кода модели удаления водяных знаков требуется пакет requests."
        ) from exc

    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_name(dest.name + ".part")
    digest = hashlib.sha256()
    written = 0
    try:
        with requests.get(
            url, stream=True, allow_redirects=True, timeout=_DOWNLOAD_TIMEOUT_S
        ) as response:
            response.raise_for_status()
            with tmp.open("wb") as handle:
                for chunk in response.iter_content(chunk_size=1 << 16):
                    if not chunk:
                        continue
                    handle.write(chunk)
                    digest.update(chunk)
                    written += len(chunk)
                    on_chunk(written)
    except Exception as exc:
        _unlink_quietly(tmp)
        # One message for both failure modes on purpose: `requests`' exceptions
        # derive from `OSError`, so a transport error and a disk error cannot be
        # told apart here. Both operands are named so the log stays actionable.
        raise RuntimeError(
            f"Не удалось получить файл кода модели.\nИсточник: {url}\nФайл: {dest}\n"
            f"Ошибка: {exc}"
        ) from exc

    actual = digest.hexdigest()
    if written != entry.size or actual != entry.sha256:
        _unlink_quietly(tmp)
        log.error(
            "watermark: integrity check failed for %s (expected %d bytes / %s, "
            "got %d bytes / %s)",
            url,
            entry.size,
            entry.sha256,
            written,
            actual,
        )
        raise RuntimeError(
            "Проверка целостности скачанного кода не пройдена — файл не будет "
            f"использован.\nФайл: {entry.path}\nИсточник: {url}\n"
            f"Ожидалось: {entry.size} байт, SHA-256 {entry.sha256}\n"
            f"Получено:  {written} байт, SHA-256 {actual}"
        )

    os.replace(tmp, dest)


def _file_matches(path: Path, entry: _RemoteFile) -> bool:
    """Whether `path` exists with exactly the manifest's size and digest."""
    try:
        if not path.is_file() or path.stat().st_size != entry.size:
            return False
        return _sha256_file(path) == entry.sha256
    except OSError:
        return False


def _sha256_file(path: Path) -> str:
    """Hex SHA-256 of `path`, read in 1 MiB chunks."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _unlink_quietly(path: Path) -> None:
    """Remove `path`, ignoring the case where it is already gone."""
    try:
        path.unlink()
    except FileNotFoundError:
        return
    except OSError as exc:
        log.warning("watermark: could not remove partial file %s: %s", path, exc)


def _validate_model_id(model_id: str) -> str:
    """Return `model_id` unchanged, or raise for anything outside `MODEL_IDS`."""
    if model_id not in _REPOS:
        raise ValueError(
            f"Неизвестная модель удаления водяных знаков: {model_id!r}. "
            f"Доступны: {', '.join(MODEL_IDS)}"
        )
    return model_id
