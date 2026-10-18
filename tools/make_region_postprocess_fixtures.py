"""Regenerate the golden region post-processing fixtures of `ms-region-postprocess`.

Why this script exists
----------------------
The FLUX.2 / FLUX.3 klein region post-processing (global mean/std colour match,
local CIELAB match, inward feathered composite, cv2 ellipse mask dilation) moves
from the Python backend (`modules/ai_backend/inpaint/region_imaging.py`) into the
Rust crate `crates/ms-region-postprocess`, and the Python helpers are deleted.
This script records what that Python code produces on small SYNTHETIC inputs, so
the Rust port is parity-tested against it instead of hand-written expectations.

The reference code is read from git, never from the working tree:
`git show c11c4eb:modules/ai_backend/inpaint/region_imaging.py` is executed into a
private module object (twice: one patched, one untouched copy). The script keeps
working after the module is gone from the tree.

The two sets (stage subdirectories of every case)
-------------------------------------------------
- `patched/` — the parity target: the reference with the Rust port's two DELIBERATE deviations
  applied, so parity can be tight:
  1. exact EDT: `mask_distance_inside` uses
     `cv2.distanceTransform(DIST_L2, DIST_MASK_PRECISE)` (exact Euclidean) on the
     same 1 px zero-padded mask, instead of the `DIST_L2` 5x5 chamfer;
  2. splitmix jitter: `_jittered_sample_indices` picks the per-block sample with
     the splitmix64 rule below instead of numpy's `default_rng(level)` (PCG64,
     not reproducible cheaply in Rust).
  In addition, the local match's four low-pass helpers (`_mix_channels`,
  `_box_downsample`, `_upsample_linear`, `_gaussian_blur`) run their NUMPY
  FALLBACK paths (cv2 is hidden from them), because those are the paths the Rust
  port implements literally. Mask dilation stays real cv2 (`MORPH_ELLIPSE`).
  `case.json` records how far the cv2 helper paths are from the fallbacks
  (`patched_diagnostics`).
- `unpatched/` — the untouched reference (chamfer distance, PCG64 jitter, cv2
  helper paths): today's real output, used only to report the deviation;
  `case.json` records the per-stage difference against the patched set
  (`unpatched_deviation`). It has no `global.png`: the global stage depends on
  neither patch and is asserted byte-identical to the patched one.

splitmix jitter rule (level = cascade level i >= 1, stride = 2**i, padded_h /
padded_w = the padded field size the indices address)::

    key = (level << 48) ^ (block_row << 24) ^ block_col
    z = key + 0x9E3779B97F4A7C15
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
    z = (z ^ (z >> 27)) * 0x94D049BB133111EB
    h = z ^ (z >> 31)                       # all arithmetic wrapping u64
    row = min(block_row * stride + (h & 0xFFFFFFFF) % stride, padded_h - 1)
    col = min(block_col * stride + (h >> 32) % stride,        padded_w - 1)

Outputs (under `crates/ms-region-postprocess/fixtures/`, layout in its README.md):
`manifest.json`, `kernels.json` (cv2 ellipse row half-widths, r = 1..=64),
`jitter.json` (splitmix64 vectors), `dilation/` (cv2 dilations of one
border-touching mask) and `cases/<case>/` (inputs + `case.json`, stage outputs in
`patched/` and `unpatched/`).

Usage
-----
    venv/bin/python -I tools/make_region_postprocess_fixtures.py

Run from anywhere; paths resolve from this file. Deletes and rewrites the
generated parts of the fixtures directory (never its README.md). Output is
byte-deterministic for a given numpy / OpenCV / Pillow version (recorded in
`manifest.json`): two runs give identical sha256 for every file. Exit `0` =
written, `1` = a case broke a contract it was built to exercise, `2` = git or the
reference revision is unavailable. Offline developer tool; nothing imports it.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import logging
import shutil
import subprocess
import sys
import types
from collections.abc import Iterator
from dataclasses import asdict, dataclass
from pathlib import Path

import cv2
import numpy as np
import PIL
from PIL import Image

log = logging.getLogger("make_region_postprocess_fixtures")

REPO_ROOT = Path(__file__).resolve().parents[1]
OUT_DIR = REPO_ROOT / "crates" / "ms-region-postprocess" / "fixtures"
REFERENCE_REV = "c11c4eb"
REFERENCE_PATH = "modules/ai_backend/inpaint/region_imaging.py"
#: Generated entries of `OUT_DIR`; everything else there (README.md) is hand-written.
GENERATED = ("manifest.json", "kernels.json", "jitter.json", "dilation", "cases")
STAGES = ("global", "local", "alpha", "expected")
U64 = (1 << 64) - 1
KERNEL_MAX_RADIUS = 64
DILATION_RADII = (3, 6, 16)
#: Mirrors of the reference constants used to classify each case's fallback path.
MIN_SAMPLES = 256
GLOBAL_MARGIN_PX = 6
LOCAL_MARGIN_PX = 3


class FixtureError(RuntimeError):
    """A generated case does not exercise the contract it was built for."""


# ---------------------------------------------------------------------
#  Reference loading and the patches
# ---------------------------------------------------------------------
def reference_source() -> bytes:
    """Return the reference module's bytes at `REFERENCE_REV` (raises CalledProcessError / OSError)."""
    return subprocess.run(
        ["git", "-C", str(REPO_ROOT), "show", f"{REFERENCE_REV}:{REFERENCE_PATH}"],
        check=True,
        capture_output=True,
    ).stdout


def load_reference(source: bytes, name: str) -> types.ModuleType:
    """Execute `source` into a fresh, registered module called `name`."""
    module = types.ModuleType(name)
    module.__file__ = f"<{REFERENCE_REV}:{REFERENCE_PATH}>"
    sys.modules[name] = module
    exec(compile(source, module.__file__, "exec"), module.__dict__)
    return module


def splitmix64(value: int) -> int:
    """splitmix64 finalizer of `value` (wrapping u64), the Rust port's jitter hash."""
    z = (value + 0x9E3779B97F4A7C15) & U64
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & U64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & U64
    return z ^ (z >> 31)


def splitmix_jitter(height: int, width: int, stride: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """Drop-in for the reference `_jittered_sample_indices`: (rows x cols) index grids, splitmix rule."""
    rows = -(-height // stride)
    cols = -(-width // stride)
    row_index = np.empty((rows, cols), dtype=np.int64)
    col_index = np.empty((rows, cols), dtype=np.int64)
    for block_row in range(rows):
        for block_col in range(cols):
            h = splitmix64((seed << 48) ^ (block_row << 24) ^ block_col)
            row_index[block_row, block_col] = min(block_row * stride + (h & 0xFFFFFFFF) % stride, height - 1)
            col_index[block_row, block_col] = min(block_col * stride + (h >> 32) % stride, width - 1)
    return row_index, col_index


def precise_distance_inside(mask: np.ndarray) -> np.ndarray:
    """Exact-EDT drop-in for the reference `mask_distance_inside` (same 1 px zero ring)."""
    binary = np.pad((mask > 0).astype(np.uint8), 1)
    return np.ascontiguousarray(cv2.distanceTransform(binary, cv2.DIST_L2, cv2.DIST_MASK_PRECISE)[1:-1, 1:-1])


def cv2_morph(mask: np.ndarray, radius: int, *, grow: bool) -> np.ndarray:
    """The reference `morph_mask` cv2 path, bound to this script's cv2 so it survives `hidden_cv2`."""
    if radius <= 0:
        return mask
    kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2 * radius + 1, 2 * radius + 1))
    return (cv2.dilate if grow else cv2.erode)(mask, kernel, iterations=1)


def ellipse_dilate(mask: np.ndarray, radius: int) -> np.ndarray:
    """cv2 MORPH_ELLIPSE dilation, kernel 2r+1 (the contract `dilate_ellipse` ports)."""
    return cv2_morph(mask, radius, grow=True)


def patch(module: types.ModuleType) -> None:
    """Apply the Rust port's deliberate deviations to a loaded reference module."""
    module.mask_distance_inside = precise_distance_inside
    module._jittered_sample_indices = splitmix_jitter
    module.morph_mask = cv2_morph


@contextlib.contextmanager
def hidden_cv2(active: bool) -> Iterator[None]:
    """While active, make `import cv2` inside the reference raise ImportError (numpy fallbacks run)."""
    if not active:
        yield
        return
    saved = sys.modules.get("cv2")
    sys.modules["cv2"] = None  # type: ignore[assignment]  # the documented way to block an import
    try:
        yield
    finally:
        if saved is None:
            del sys.modules["cv2"]
        else:
            sys.modules["cv2"] = saved


# ---------------------------------------------------------------------
#  Synthetic cases
# ---------------------------------------------------------------------
@dataclass(frozen=True)
class Case:
    """One fixture case: inputs, stored settings and the fallback paths it must hit."""

    name: str
    base: np.ndarray
    generated: np.ndarray
    mask: np.ndarray
    sample_mask: np.ndarray | None
    reference: np.ndarray | None
    feather_px: int
    color_match: bool
    local_pct: int
    scale_px: int
    whole_region: bool
    expect_global: str
    expect_local: str
    sample_mask_rule: str = "mask"


def synth_base(height: int, width: int, seed: int) -> np.ndarray:
    """A page-like RGB region: gradient background, flat surfaces, ink lines, screentone, speckle."""
    rng = np.random.default_rng(seed)
    yy, xx = np.mgrid[0:height, 0:width].astype(np.float32)
    start = rng.uniform(90, 200, 3).astype(np.float32)
    slope = rng.uniform(-0.4, 0.4, (2, 3)).astype(np.float32)
    img = start + yy[..., None] * slope[0] + xx[..., None] * slope[1]
    img += 10.0 * np.sin(xx / 23.0 + yy / 31.0)[..., None]
    for _ in range(3):
        cy, cx = int(rng.integers(0, height)), int(rng.integers(0, width))
        ry, rx = int(rng.integers(8, height // 3 + 9)), int(rng.integers(8, width // 3 + 9))
        surface = ((yy - cy) / ry) ** 2 + ((xx - cx) / rx) ** 2 <= 1.0
        img[surface] = rng.uniform(40, 235, 3).astype(np.float32)
    out = np.clip(np.rint(img), 0, 255).astype(np.uint8)
    for _ in range(4):
        p0 = (int(rng.integers(0, width)), int(rng.integers(0, height)))
        p1 = (int(rng.integers(0, width)), int(rng.integers(0, height)))
        cv2.line(out, p0, p1, (20, 18, 25), 2, lineType=cv2.LINE_8)
    # Texture without random noise (noise would dominate the PNG size): a period-4
    # screentone over the left half and sparse +-1 speckle, so the local statistics
    # see a periodic pattern (the jitter's reason to exist) and odd rounding ties.
    tone = ((yy.astype(np.int32) % 4 == 0) & (xx.astype(np.int32) % 4 == 0) & (xx < width / 2))[..., None]
    speckle = rng.integers(-1, 2, out.shape) * (rng.random((height, width, 1)) < 0.04)
    return np.clip(out.astype(np.int16) - 30 * tone + speckle, 0, 255).astype(np.uint8)


def synth_generated(base: np.ndarray, mask: np.ndarray, seed: int) -> np.ndarray:
    """A VAE-like round trip (tone shift + noise) with a drifting, line-free fill inside `mask`."""
    rng = np.random.default_rng(seed)
    out = base.astype(np.float32) * rng.uniform(0.9, 0.97, 3).astype(np.float32) + rng.uniform(-6, 12, 3).astype(np.float32)
    inside = mask > 0
    if inside.any():
        fill = cv2.GaussianBlur(out, (0, 0), 6.0)
        depth = cv2.distanceTransform(np.pad(inside.astype(np.uint8), 1), cv2.DIST_L2, cv2.DIST_MASK_PRECISE)[1:-1, 1:-1]
        drift = (depth / max(float(depth.max()), 1.0))[..., None] * rng.uniform(-18, 18, 3).astype(np.float32)
        out[inside] = (fill + drift)[inside]
    return np.clip(np.rint(out), 0, 255).astype(np.uint8)


def blob_mask(height: int, width: int, ellipses: list[tuple[int, int, int, int]]) -> np.ndarray:
    """0/255 L8 union of axis-aligned ellipses (cy, cx, ry, rx)."""
    yy, xx = np.mgrid[0:height, 0:width]
    mask = np.zeros((height, width), dtype=np.uint8)
    for cy, cx, ry, rx in ellipses:
        mask[((yy - cy) / ry) ** 2 + ((xx - cx) / rx) ** 2 <= 1.0] = 255
    return mask


def ring_mask(height: int, width: int, ring_px: int) -> np.ndarray:
    """0/255 mask covering everything but a `ring_px` wide border ring."""
    mask = np.zeros((height, width), dtype=np.uint8)
    mask[ring_px : height - ring_px, ring_px : width - ring_px] = 255
    return mask


def build_cases() -> list[Case]:
    """The fixture cases: plan WP-2 cases 1-6 (the fallback case split into its two branches)
    plus `07_marks_overlay`, whose colour-match reference (the marked region the model was sent)
    differs from the composite base (the unmarked original), with feather 0 (hard-edge branch)."""
    cases: list[Case] = []

    def add(name: str, h: int, w: int, mask: np.ndarray, seed: int, *, feather_px: int, color_match: bool, local_pct: int, scale_px: int,
            whole_region: bool, expect_global: str, expect_local: str, sample_mask: np.ndarray | None = None, sample_mask_rule: str = "mask") -> None:
        """Append a case whose base is `synth_base(seed)` and whose generated image is derived from it."""
        base = synth_base(h, w, seed)
        cases.append(Case(name=name, base=base, generated=synth_generated(base, mask, seed + 100), mask=mask, sample_mask=sample_mask, reference=None,
                          feather_px=feather_px, color_match=color_match, local_pct=local_pct, scale_px=scale_px, whole_region=whole_region,
                          expect_global=expect_global, expect_local=expect_local, sample_mask_rule=sample_mask_rule))

    add("01_blob", 160, 192, blob_mask(160, 192, [(80, 90, 38, 50), (60, 130, 22, 30)]), 1,
        feather_px=12, color_match=True, local_pct=60, scale_px=24, whole_region=False, expect_global="margin", expect_local="applied")
    add("02_border_blob", 96, 256, blob_mask(96, 256, [(0, 256, 70, 110), (10, 150, 30, 40)]), 2,
        feather_px=32, color_match=True, local_pct=100, scale_px=8, whole_region=False, expect_global="margin", expect_local="applied")
    stroke = np.zeros((160, 160), dtype=np.uint8)
    cv2.polylines(stroke, [np.array([[20, 30], [70, 120], [110, 40], [145, 140]], dtype=np.int32)], False, 255, 7, lineType=cv2.LINE_8)
    add("03_thin_stroke", 160, 160, stroke, 3,
        feather_px=12, color_match=True, local_pct=80, scale_px=16, whole_region=False, expect_global="margin", expect_local="applied")
    flux2 = blob_mask(176, 224, [(90, 110, 40, 55)])
    add("04_flux2_latent", 176, 224, flux2, 4, sample_mask=ellipse_dilate(flux2, 16), sample_mask_rule="dilate_ellipse(mask, 16)",
        feather_px=12, color_match=True, local_pct=60, scale_px=96, whole_region=False, expect_global="margin", expect_local="applied")
    add("05_whole_region", 96, 128, np.full((96, 128), 255, dtype=np.uint8), 5,
        feather_px=12, color_match=True, local_pct=60, scale_px=24, whole_region=True, expect_global="off", expect_local="off")
    add("06a_ring_unmargined", 64, 64, ring_mask(64, 64, 3), 6,
        feather_px=8, color_match=True, local_pct=60, scale_px=24, whole_region=False, expect_global="unmargined", expect_local="skipped")
    add("06b_ring_skipped", 40, 40, ring_mask(40, 40, 1), 7,
        feather_px=8, color_match=True, local_pct=60, scale_px=24, whole_region=False, expect_global="skipped", expect_local="skipped")
    marks_mask = blob_mask(112, 128, [(56, 64, 30, 40)])
    marks_base = synth_base(112, 128, 8)
    marked = marks_base.copy()
    cv2.line(marked, (30, 40), (100, 70), (230, 30, 30), 3, lineType=cv2.LINE_8)
    cv2.circle(marked, (64, 56), 12, (230, 30, 30), 2, lineType=cv2.LINE_8)
    cases.append(Case(name="07_marks_overlay", base=marks_base, generated=synth_generated(marked, marks_mask, 108), mask=marks_mask, sample_mask=None, reference=marked,
                      feather_px=0, color_match=True, local_pct=60, scale_px=24, whole_region=False, expect_global="margin", expect_local="applied"))
    return cases


# ---------------------------------------------------------------------
#  Running a case
# ---------------------------------------------------------------------
@dataclass(frozen=True)
class Effective:
    """Effective settings of a case: the rule the Rust `PostprocessSettings::effective` owns."""

    feather_px: int
    global_match: bool
    local_strength_pct: int
    local_scale_px: int


def effective(case: Case) -> Effective:
    """No colour match without a ring (`whole_region`); the local match only under the global one."""
    global_match = case.color_match and not case.whole_region
    return Effective(case.feather_px, global_match, case.local_pct if global_match else 0, case.scale_px)


def classify_paths(case: Case) -> dict[str, object]:
    """Which global / local branch the reference takes, with the sample counts that decide it."""
    eff = effective(case)
    sample = case.sample_mask if case.sample_mask is not None else case.mask
    margin = int(np.count_nonzero(ellipse_dilate(sample, GLOBAL_MARGIN_PX) == 0))
    unmargined = int(np.count_nonzero(sample == 0))
    valid = int(np.count_nonzero(ellipse_dilate(sample, LOCAL_MARGIN_PX) == 0))
    if not eff.global_match:
        global_path = "off"
    elif margin >= MIN_SAMPLES:
        global_path = "margin"
    elif unmargined >= MIN_SAMPLES:
        global_path = "unmargined"
    else:
        global_path = "skipped"
    if not eff.local_strength_pct:
        local_path = "off"
    else:
        local_path = "applied" if valid >= MIN_SAMPLES and np.any(sample > 0) else "skipped"
    return {"global_path": global_path, "local_path": local_path, "global_margin_samples": margin, "global_unmargined_samples": unmargined, "local_valid_samples": valid}


def run_stages(module: types.ModuleType, case: Case, *, fallbacks: bool) -> dict[str, np.ndarray]:
    """Run global -> local -> feather -> composite through `module`; returns every stage output."""
    eff = effective(case)
    reference = case.reference if case.reference is not None else case.base
    sample = case.sample_mask if case.sample_mask is not None else case.mask
    with hidden_cv2(fallbacks):
        matched = module.match_color_outside_mask(case.generated, reference, sample) if eff.global_match else case.generated
        pct = eff.local_strength_pct
        local = module.match_color_local(reference, matched, sample, strength=pct / 100.0, scale_px=case.scale_px) if pct else matched
        alpha = np.where(case.mask > 0, module.feather_mask_inwards(case.mask, case.feather_px), 0).astype(np.uint8)
        expected = module.composite_over_region(case.base, local, case.mask, case.feather_px)
    return {"global": np.asarray(matched), "local": np.asarray(local), "alpha": alpha, "expected": np.asarray(expected)}


def check_contracts(case: Case, stages: dict[str, np.ndarray], paths: dict[str, object]) -> None:
    """Raise FixtureError when a case misses its intended branch or breaks the outside-mask contract."""
    if (paths["global_path"], paths["local_path"]) != (case.expect_global, case.expect_local):
        raise FixtureError(f"{case.name}: paths {paths['global_path']}/{paths['local_path']}, built for {case.expect_global}/{case.expect_local}")
    outside = case.mask == 0
    if not np.array_equal(stages["expected"][outside], case.base[outside]):
        raise FixtureError(f"{case.name}: expected differs from base outside the mask")
    if np.any(stages["alpha"][outside]):
        raise FixtureError(f"{case.name}: alpha is non-zero outside the mask")
    sample = case.sample_mask if case.sample_mask is not None else case.mask
    untouched = sample == 0
    if not np.array_equal(stages["local"][untouched], stages["global"][untouched]):
        raise FixtureError(f"{case.name}: the local match changed a pixel outside the sample mask")


def diff_stats(a: np.ndarray, b: np.ndarray) -> dict[str, int]:
    """Max |a-b|, the number of differing values and the 99th percentile of |a-b| (integers)."""
    delta = np.abs(a.astype(np.int16) - b.astype(np.int16))
    return {"max_abs": int(delta.max(initial=0)), "differing_values": int(np.count_nonzero(delta)), "p99_abs": int(np.percentile(delta, 99, method="higher")) if delta.size else 0}


# ---------------------------------------------------------------------
#  Writing
# ---------------------------------------------------------------------
def write_png(path: Path, array: np.ndarray) -> None:
    """Write an HxW (L8) or HxWx3 (RGB) uint8 array as a metadata-free PNG."""
    mode = "L" if array.ndim == 2 else "RGB"
    Image.fromarray(np.ascontiguousarray(array, dtype=np.uint8), mode).save(path, format="PNG", compress_level=9)


def write_json(path: Path, value: object) -> None:
    """Write `value` as sorted, indented JSON with a trailing newline."""
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_inputs(directory: Path, case: Case, meta: dict[str, object]) -> None:
    """Write a case's inputs and its case.json into `directory` (created)."""
    directory.mkdir(parents=True)
    write_png(directory / "base.png", case.base)
    write_png(directory / "generated.png", case.generated)
    write_png(directory / "mask.png", case.mask)
    write_png(directory / "sample_mask.png", case.sample_mask if case.sample_mask is not None else case.mask)
    if case.reference is not None:
        write_png(directory / "reference.png", case.reference)
    write_json(directory / "case.json", meta)


def write_stages(directory: Path, stages: dict[str, np.ndarray], names: tuple[str, ...]) -> None:
    """Write the named stage outputs of one set into `directory` (created)."""
    directory.mkdir(parents=True)
    for stage in names:
        write_png(directory / f"{stage}.png", stages[stage])


def kernel_table() -> tuple[dict[str, object], int]:
    """cv2 MORPH_ELLIPSE row half-widths for r = 1..=KERNEL_MAX_RADIUS and the count of rows where
    the plan formula `round_half_even(r * sqrt((r^2 - dy^2) / r^2))` disagrees (listed in the table)."""
    rows: list[dict[str, object]] = []
    mismatches: list[dict[str, int]] = []
    for radius in range(1, KERNEL_MAX_RADIUS + 1):
        kernel = cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2 * radius + 1, 2 * radius + 1))
        half_widths: list[int] = []
        for dy, row in zip(range(-radius, radius + 1), kernel):
            columns = np.flatnonzero(row) - radius
            half = int(columns.max())
            if not np.array_equal(columns, np.arange(-half, half + 1)):
                raise FixtureError(f"ellipse kernel r={radius} dy={dy} is not a centred contiguous span")
            half_widths.append(half)
            formula = round(radius * ((radius * radius - dy * dy) / (radius * radius)) ** 0.5)
            if formula != half:
                mismatches.append({"radius": radius, "dy": dy, "cv2": half, "plan_formula": formula})
        rows.append({"radius": radius, "half_widths": half_widths})
    table = {"source": "cv2.getStructuringElement(cv2.MORPH_ELLIPSE, (2r+1, 2r+1)); half_widths[i] is row dy = i - r", "kernels": rows, "plan_formula_mismatches": mismatches}
    return table, len(mismatches)


def dilation_input() -> np.ndarray:
    """A 97x83 0/255 mask touching all four borders, with isolated pixels and a 1 px line."""
    mask = blob_mask(83, 97, [(0, 10, 14, 20), (83, 60, 18, 25), (40, 97, 12, 9)])
    mask[0, 96] = mask[82, 0] = mask[41, 30] = 255
    mask[20:21, 40:75] = 255
    return mask


def jitter_vectors() -> dict[str, object]:
    """splitmix64 known answers and two small index grids for the Rust jitter test."""
    keys = [0, 1, (1 << 48), (2 << 48) ^ (3 << 24) ^ 5, U64]
    grids = []
    for height, width, level in ((9, 7, 1), (17, 12, 3)):
        rows, cols = splitmix_jitter(height, width, 2**level, level)
        grids.append({"padded_h": height, "padded_w": width, "level": level, "row_index": rows.tolist(), "col_index": cols.tolist()})
    return {"splitmix64": [{"key": k, "hash": splitmix64(k)} for k in keys], "grids": grids}


def generate() -> None:
    """Rebuild every generated fixture under OUT_DIR (raises FixtureError on a broken case)."""
    source = reference_source()
    patched = load_reference(source, "region_imaging_ref_patched")
    patch(patched)
    unpatched = load_reference(source, "region_imaging_ref_unpatched")
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for entry in GENERATED:
        target = OUT_DIR / entry
        if target.is_dir():
            shutil.rmtree(target)
        elif target.exists():
            target.unlink()

    cases = build_cases()
    for case in cases:
        meta: dict[str, object] = {
            "name": case.name,
            "width": int(case.mask.shape[1]),
            "height": int(case.mask.shape[0]),
            "settings": {"mask_feather_px": case.feather_px, "color_match": case.color_match, "color_match_local_pct": case.local_pct, "color_match_scale_px": case.scale_px},
            "whole_region": case.whole_region,
            "effective": asdict(effective(case)),
            "reference_distinct": case.reference is not None,
            "sample_mask_distinct": case.sample_mask is not None,
            "sample_mask_rule": case.sample_mask_rule,
            "mask_pixels": int(np.count_nonzero(case.mask)),
            **classify_paths(case),
        }
        fixed = run_stages(patched, case, fallbacks=True)
        check_contracts(case, fixed, meta)
        with_cv2 = run_stages(patched, case, fallbacks=False)
        real = run_stages(unpatched, case, fallbacks=False)
        check_contracts(case, real, meta)
        # The global stage depends on neither patch, so the unpatched set does not repeat it.
        if not np.array_equal(real["global"], fixed["global"]):
            raise FixtureError(f"{case.name}: the global stage differs between the sets")
        meta["patched_diagnostics"] = {f"{k}_cv2_helpers_vs_fallbacks": diff_stats(with_cv2[k], fixed[k]) for k in ("local", "expected")}
        meta["unpatched_deviation"] = {k: diff_stats(real[k], fixed[k]) for k in STAGES[1:]}
        directory = OUT_DIR / "cases" / case.name
        write_inputs(directory, case, meta)
        write_stages(directory / "patched", fixed, STAGES)
        write_stages(directory / "unpatched", real, STAGES[1:])
        log.info("case %s: %s/%s, unpatched expected max |d| %d", case.name, meta["global_path"], meta["local_path"], diff_stats(real["expected"], fixed["expected"])["max_abs"])

    kernels, mismatches = kernel_table()
    if mismatches:
        log.warning("cv2 ellipse kernel differs from the plan formula at %d rows (recorded in kernels.json).", mismatches)
    write_json(OUT_DIR / "kernels.json", kernels)
    write_json(OUT_DIR / "jitter.json", jitter_vectors())
    dilation = OUT_DIR / "dilation"
    dilation.mkdir()
    source_mask = dilation_input()
    write_png(dilation / "input.png", source_mask)
    for radius in DILATION_RADII:
        write_png(dilation / f"dilate_r{radius}.png", ellipse_dilate(source_mask, radius))
    write_json(OUT_DIR / "manifest.json", {
        "reference": f"{REFERENCE_REV}:{REFERENCE_PATH}",
        "reference_sha256": hashlib.sha256(source).hexdigest(),
        "generator": "tools/make_region_postprocess_fixtures.py",
        "versions": {"python": sys.version.split()[0], "numpy": np.__version__, "opencv": cv2.__version__, "pillow": PIL.__version__},
        "cases": [case.name for case in cases],
        "dilation_radii": list(DILATION_RADII),
    })


def main() -> int:
    """Entry point: returns the process exit status (see the module docstring)."""
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    try:
        generate()
    except subprocess.CalledProcessError as error:
        log.error("Cannot read the reference %s:%s from git (exit %s): %s", REFERENCE_REV, REFERENCE_PATH, error.returncode, error.stderr.decode(errors="replace").strip())
        return 2
    except FileNotFoundError as error:
        log.error("git is not available: %s", error)
        return 2
    except FixtureError as error:
        log.error("Fixture contract broken: %s", error)
        return 1
    log.info("Fixtures written to %s", OUT_DIR)
    return 0


if __name__ == "__main__":
    sys.exit(main())
