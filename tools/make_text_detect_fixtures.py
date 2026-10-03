"""Regenerate the golden text-detector postprocess fixtures of `ms-text-detect`.

Why this script exists
----------------------
The CTD, PaddleOCR-det and Surya-det postprocessing moves from the Python AI
backend into Rust (`crates/ms-text-detect`). Before the Python postprocess is
deleted, this script records what the CURRENT Python code produces for a set of
small synthetic inputs, so the Rust ports can be parity-tested against it.

No model weights are needed. The network forward pass is replaced by a fake
that returns a probability map stored next to the fixture; everything after the
forward pass is the real backend code, called through the same service entry
points the IPC handlers use:

- CTD: `CtdTextDetectorService._detect_from_encoded_image_bytes` with the real
  `TextDetector` (letterbox, DB postprocess, `group_output`, `refine_mask`),
  whose `load_model` is overridden to install a fake net.
- Paddle: `PaddleTextDetectorService._detect_from_encoded_bytes` with a fake
  runtime whose `detect` runs the real `DBPostProcess` (configured by the real
  `parse_det_config` defaults) on the fixture map; the real glyph mask,
  block collection and PNG encoding follow.
- Surya: `SuryaTextDetectorService._detect_with_predictor` with a fake
  predictor that yields the fixture heatmap at processor size.

Intermediate values (DB candidates, refine windows and candidates, Surya
dynamic thresholds, Paddle glyph-mask branches) are captured with in-process
wrappers around the backend functions; no backend file is modified.

Maps are stored as u8 PNGs and fed to Python as `float32(u8) / float32(255)`,
so the quantisation Rust sees is identical. The fixture format is documented in
`crates/ms-text-detect/fixtures/README.md`.

Usage
-----
    ./venv/bin/python tools/make_text_detect_fixtures.py

Rewrites `crates/ms-text-detect/fixtures/{ctd,paddle,surya}/` and
`crates/ms-text-detect/fixtures/manifest.json` (the manifest records the git
revision the reference code was taken from and the package versions). Output is
deterministic: two runs on the same revision and environment are byte-identical.
Exits with status 2 and a clear message when the reference backend modules are
absent (they are removed once the backend becomes forward-only).

Notes
-----
Importing the backend runs the root `config.py`, which creates the
`ManhwaStudio_AI_Models/` folders if missing and reads `user_config` read-only;
this script itself writes only under the fixtures directory. It is an offline
developer tool and must never be imported by runtime code.
"""

from __future__ import annotations

import contextlib
import hashlib
import importlib
import json
import logging
import shutil
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Callable, Iterator
from unittest import mock

log = logging.getLogger("make_text_detect_fixtures")

try:
    import numpy as np
except ModuleNotFoundError as _numpy_missing:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    log.error("numpy is not installed in this interpreter (%s).\nRun the script with ./venv/bin/python.", sys.executable)
    raise SystemExit(2) from _numpy_missing

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURES_DIR = REPO_ROOT / "crates/ms-text-detect/fixtures"
ENGINE_DIRS = ("ctd", "paddle", "surya")

# The reference modules this script drives. Their absence means the Python
# postprocess has already been removed; the fixtures must then be kept as-is.
REFERENCE_MODULES = (
    "modules.ai_backend.detection.ctd",
    "modules.ai_backend.detection.textdetector.ctd.inference",
    "modules.ai_backend.detection.textdetector.ctd.textmask",
    "modules.ai_backend.detection.textdetector.td_utlis",
    "modules.ai_backend.detection.paddle",
    "modules.ai_backend.engines.paddle_onnx",
    "modules.ai_backend.detection.surya",
)

# Paths whose uncommitted edits would make the recorded git revision misleading.
REFERENCE_PATHS = (
    "modules/ai_backend/detection",
    "modules/ai_backend/engines/paddle_onnx.py",
)

# Margins that keep the fixtures robust: a score, threshold or fill fraction
# this close to its decision boundary would make the parity test flip on an
# ulp-level or polygon-fill difference instead of on a real porting bug.
SCORE_MARGIN = 0.02
THRESHOLD_LEVEL_MARGIN = 1e-5
GLYPH_FILL_MARGIN = 0.005
GLYPH_SAT_MARGIN = 1.0


class FixtureError(RuntimeError):
    """A generated case violates a fixture robustness or consistency rule."""


# ---------------------------------------------------------------------------
# Deterministic file output
# ---------------------------------------------------------------------------


def write_png(path: Path, array: np.ndarray) -> None:
    """Write a u8 gray (H,W) or RGB (H,W,3) array as a deterministic PNG."""
    from PIL import Image

    if array.dtype != np.uint8:
        raise FixtureError(f"PNG {path} must be u8, got {array.dtype}")
    mode = "L" if array.ndim == 2 else "RGB"
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(np.ascontiguousarray(array), mode=mode).save(path, format="PNG", compress_level=9)


def encode_png(array: np.ndarray) -> bytes:
    """Encode an RGB array exactly as `write_png` stores it (bytes fed to the services)."""
    import io

    from PIL import Image

    buffer = io.BytesIO()
    Image.fromarray(np.ascontiguousarray(array), mode="RGB").save(buffer, format="PNG", compress_level=9)
    return buffer.getvalue()


def to_jsonable(value: Any) -> Any:
    """Convert numpy scalars/arrays (recursively) to plain JSON values."""
    if isinstance(value, dict):
        return {str(k): to_jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [to_jsonable(v) for v in value]
    if isinstance(value, np.ndarray):
        return to_jsonable(value.tolist())
    if isinstance(value, np.bool_):
        return bool(value)
    if isinstance(value, np.integer):
        return int(value)
    if isinstance(value, np.floating):
        return float(value)
    return value


def write_json(path: Path, payload: dict[str, Any]) -> None:
    """Write JSON with sorted keys and a trailing newline (deterministic bytes)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(to_jsonable(payload), indent=1, sort_keys=True, ensure_ascii=True)
    path.write_text(text + "\n", encoding="utf-8")


def prob_from_u8(map_u8: np.ndarray) -> np.ndarray:
    """The one quantisation contract: u8 level k -> float32(k) / float32(255)."""
    return map_u8.astype(np.float32) / np.float32(255.0)


def u8_from_prob(prob: np.ndarray) -> np.ndarray:
    """Quantise a synthetic float map in [0,1] to the stored u8 levels."""
    return np.clip(np.rint(prob * 255.0), 0, 255).astype(np.uint8)


# ---------------------------------------------------------------------------
# Synthetic scenes
# ---------------------------------------------------------------------------

# Stroke templates of the fake glyphs, as segments in a unit cell (x right, y down).
GLYPH_SEGMENTS = (
    ((0.0, 0.0), (1.0, 0.0)),
    ((0.0, 0.0), (0.0, 1.0)),
    ((1.0, 0.0), (1.0, 1.0)),
    ((0.0, 1.0), (1.0, 1.0)),
    ((0.0, 0.0), (1.0, 1.0)),
    ((1.0, 0.0), (0.0, 1.0)),
    ((0.5, 0.0), (0.5, 1.0)),
    ((0.0, 0.5), (1.0, 0.5)),
)


@dataclass(frozen=True)
class TextLine:
    """One synthetic text line, in source pixels.

    `center`/`angle_deg` place the line; `chars` glyph cells of `char_w` x
    `char_h` px are laid out with `pitch` px advance along the line direction.
    `color` is the RGB stroke colour. `map_peak` is the probability the fake
    network assigns to the line (shrink / probability / heat map); `map_shrink`
    is how many px the map rectangle is shrunk on each side of the glyph box.
    """

    center: tuple[float, float]
    angle_deg: float
    chars: int
    char_w: float
    char_h: float
    pitch: float
    color: tuple[int, int, int]
    thickness: int = 2
    map_peak: float = 0.95
    map_shrink: float = 1.0
    draw_text: bool = True


def line_axes(line: TextLine) -> tuple[np.ndarray, np.ndarray, np.ndarray, float]:
    """Return (top-left corner, unit along, unit down, line length) of a line."""
    theta = np.deg2rad(line.angle_deg)
    along = np.array([np.cos(theta), np.sin(theta)])
    down = np.array([-np.sin(theta), np.cos(theta)])
    length = line.chars * line.pitch - (line.pitch - line.char_w)
    center = np.array(line.center, dtype=np.float64)
    top_left = center - along * (length / 2.0) - down * (line.char_h / 2.0)
    return top_left, along, down, length


def line_quad(line: TextLine, shrink: float) -> np.ndarray:
    """The line's oriented glyph box shrunk by `shrink` px per side, (4,2) source px."""
    top_left, along, down, length = line_axes(line)
    p0 = top_left + along * shrink + down * shrink
    a = along * (length - 2.0 * shrink)
    d = down * (line.char_h - 2.0 * shrink)
    return np.array([p0, p0 + a, p0 + a + d, p0 + d])


def draw_text_line(image: np.ndarray, strokes: np.ndarray, line: TextLine, rng: np.random.Generator) -> None:
    """Draw fake anti-aliased glyphs into `image` and their binary strokes into `strokes`."""
    import cv2

    top_left, along, down, _ = line_axes(line)
    shift = 4
    scale = float(1 << shift)
    for index in range(line.chars):
        origin = top_left + along * (index * line.pitch)
        count = int(rng.integers(2, 4))
        picks = rng.choice(len(GLYPH_SEGMENTS), size=count, replace=False)
        for pick in sorted(int(p) for p in picks):
            (a0, b0), (a1, b1) = GLYPH_SEGMENTS[pick]
            p = origin + along * (a0 * line.char_w) + down * (b0 * line.char_h)
            q = origin + along * (a1 * line.char_w) + down * (b1 * line.char_h)
            pi = (int(round(p[0] * scale)), int(round(p[1] * scale)))
            qi = (int(round(q[0] * scale)), int(round(q[1] * scale)))
            cv2.line(image, pi, qi, line.color, line.thickness, cv2.LINE_AA, shift)
            cv2.line(strokes, pi, qi, 255, line.thickness, cv2.LINE_8, shift)


def gradient_background(width: int, height: int, left: tuple[int, int, int], right: tuple[int, int, int], vertical: float) -> np.ndarray:
    """Horizontal RGB gradient with an additive vertical ramp of `vertical` levels."""
    xs = np.linspace(0.0, 1.0, width)[None, :, None]
    ys = np.linspace(-0.5, 0.5, height)[:, None, None]
    lo = np.array(left, dtype=np.float64)[None, None, :]
    hi = np.array(right, dtype=np.float64)[None, None, :]
    image = lo + (hi - lo) * xs + vertical * ys
    return np.clip(np.rint(image), 0, 255).astype(np.uint8)


def soft_quads_map(size_wh: tuple[int, int], quads: list[tuple[np.ndarray, float]], sigma: float, floor: float = 0.0) -> np.ndarray:
    """Float map with each (quad in map px, peak) filled and Gaussian-softened; max-combined."""
    import cv2

    width, height = size_wh
    out = np.full((height, width), floor, dtype=np.float64)
    for quad, peak in quads:
        layer = np.zeros((height, width), dtype=np.float64)
        points = np.rint(quad * 16.0).astype(np.int32).reshape(1, -1, 2)
        cv2.fillPoly(layer, points, float(peak), cv2.LINE_8, 4)
        layer = cv2.GaussianBlur(layer, (0, 0), sigma)
        out = np.maximum(out, layer)
    return np.clip(out, 0.0, 1.0)


def scale_quad(quad: np.ndarray, sx: float, sy: float) -> np.ndarray:
    """Map a source-px quad into map px."""
    return quad * np.array([sx, sy])


@dataclass
class Scene:
    """A synthetic page: RGB image, binary glyph strokes and the lines that made it."""

    image: np.ndarray
    strokes: np.ndarray
    lines: list[TextLine]


def render_scene(background: np.ndarray, lines: list[TextLine], seed: int, extra: Callable[[np.ndarray], None] | None = None) -> Scene:
    """Draw `lines` (and an optional `extra` painter run first) over `background`."""
    rng = np.random.default_rng(seed)
    image = background.copy()
    if extra is not None:
        extra(image)
    strokes = np.zeros(image.shape[:2], dtype=np.uint8)
    for line in lines:
        if line.draw_text:
            draw_text_line(image, strokes, line, rng)
    return Scene(image=image, strokes=strokes, lines=lines)


def white_bubble(center: tuple[int, int], axes: tuple[int, int]) -> Callable[[np.ndarray], None]:
    """Painter of a white speech bubble with a dark outline."""
    import cv2

    def paint(image: np.ndarray) -> None:
        cv2.ellipse(image, center, axes, 0, 0, 360, (250, 250, 250), -1, cv2.LINE_AA)
        cv2.ellipse(image, center, axes, 0, 0, 360, (30, 30, 30), 2, cv2.LINE_AA)

    return paint


# ---------------------------------------------------------------------------
# Instrumentation helpers
# ---------------------------------------------------------------------------


@dataclass
class ComponentOrderCheck:
    """Verifies the label numbering of every `cv2.connectedComponentsWithStats` call.

    Observed OpenCV contract (4.12, any thread count): with connectivity 4 labels
    are numbered in raster order of each component's first pixel; with
    connectivity 8 (the block-based default algorithm) in order of the first
    pixel by the key `(y // 2, x)`. The positional call in `textmask.py`
    (`cv2.connectedComponentsWithStats(mask, connectivity, cv2.CV_16U)`) binds
    its two extra arguments to the `labels`/`stats` OUTPUT parameters, so the
    effective connectivity there is the default 8 and the label type CV_32S.
    """

    calls_by_connectivity: dict[int, int] = field(default_factory=dict)

    def wrap(self, original: Callable[..., Any]) -> Callable[..., Any]:
        """Return a wrapper that enforces the documented label order."""

        def checked(*args: Any, **kwargs: Any) -> Any:
            result = original(*args, **kwargs)
            # Python binding signature: (image, labels, stats, centroids, connectivity, ltype).
            connectivity = int(kwargs.get("connectivity", args[4] if len(args) > 4 else 8))
            labels = np.asarray(result[1])
            ys, xs = np.nonzero(labels)
            if ys.size:
                rows = ys.astype(np.int64) if connectivity == 4 else (ys // 2).astype(np.int64)
                key = rows * (labels.shape[1] + 1) + xs
                first = np.full(int(result[0]), np.iinfo(np.int64).max, dtype=np.int64)
                np.minimum.at(first, labels[ys, xs], key)
                if not np.all(np.diff(first[1:]) > 0):
                    raise FixtureError(f"connectedComponentsWithStats (connectivity {connectivity}) broke the documented label order")
            self.calls_by_connectivity[connectivity] = self.calls_by_connectivity.get(connectivity, 0) + 1
            return result

        return checked


def stable_topk_colors(color_list: np.ndarray, bins: np.ndarray, k: int, color_var: int, bin_tol: float) -> list[float]:
    """`textmask.get_topk_color` with a STABLE sort (what a Rust port would naturally do)."""
    idx = np.argsort(bins * -1, kind="stable")
    color_list, bins = color_list[idx], bins[idx]
    top_colors = [color_list[0]]
    tolerance = np.sum(bins) * bin_tol
    if len(color_list) > 1:
        for color, count in zip(color_list[1:], bins[1:]):
            if np.abs(np.array(top_colors) - color).min() > color_var:
                top_colors.append(color)
            if len(top_colors) >= k or count < tolerance:
                break
    return [float(c) for c in top_colors]


def git_revision() -> dict[str, Any]:
    """The HEAD revision and any uncommitted edits under the reference paths."""
    try:
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, check=True, capture_output=True, text=True).stdout.strip()
        dirty = subprocess.run(["git", "status", "--porcelain", "--", *REFERENCE_PATHS], cwd=REPO_ROOT, check=True, capture_output=True, text=True).stdout.splitlines()
    except (OSError, subprocess.CalledProcessError) as exc:
        log.error("Could not read the git revision.\nRepository: %s\nError: %s", REPO_ROOT, exc)
        raise
    return {"head": head, "uncommitted_reference_edits": sorted(line.rstrip() for line in dirty)}


def u8_level_margin(threshold: float) -> float:
    """Smallest |float32(k)/255 - threshold| over all u8 levels k."""
    levels = np.arange(256, dtype=np.float32) / np.float32(255.0)
    return float(np.min(np.abs(levels.astype(np.float64) - float(threshold))))


# ---------------------------------------------------------------------------
# CTD
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CtdCase:
    """One CTD fixture: scene, letterbox detect size and the fake network's extras."""

    name: str
    description: str
    scene: Scene
    detect_size: int
    # Extra shrink-map blobs (quad in SOURCE px, peak) that are not text lines.
    extra_blobs: tuple[tuple[tuple[tuple[float, float], ...], float], ...] = ()
    # Extra seg-map blobs (quad in SOURCE px, peak): false-positive mask regions.
    seg_blobs: tuple[tuple[tuple[tuple[float, float], ...], float], ...] = ()


@dataclass
class CtdRecorder:
    """Collects DB and refine intermediates while one CTD case runs."""

    db_events: list[tuple[str, Any]] = field(default_factory=list)
    windows: list[dict[str, Any]] = field(default_factory=list)
    phase: str | None = None
    seg_source: np.ndarray | None = None

    def current(self) -> dict[str, Any]:
        """The refine record of the block being processed."""
        if not self.windows:
            raise FixtureError("refine instrumentation saw a candidate before enlarge_window")
        return self.windows[-1]


def ctd_db_candidates(events: list[tuple[str, Any]]) -> list[dict[str, Any]]:
    """Fold the recorded DB call sequence into one record per contour."""
    candidates: list[dict[str, Any]] = []
    stage: str | None = None
    for kind, payload in events:
        if kind == "mini":
            points, sside = payload
            if stage == "unclipped":
                candidates[-1]["box_map_px"] = points
                candidates[-1]["sside_post"] = sside
                stage = None
                continue
            record: dict[str, Any] = {"mini_box_map_px": points, "sside_pre": sside, "skipped_sside_lt_2": sside < 2}
            candidates.append(record)
            stage = None if sside < 2 else "pre"
        elif kind == "score":
            candidates[-1]["score"] = payload
            stage = "scored"
        elif kind == "unclip":
            candidates[-1]["unclipped_map_px"] = payload
            stage = "unclipped"
    return candidates


def run_ctd_case(case: CtdCase, out_dir: Path, cc_check: ComponentOrderCheck) -> dict[str, Any]:
    """Generate one CTD fixture directory and return its manifest entry."""
    import cv2
    import torch

    ctd_service_mod = importlib.import_module("modules.ai_backend.detection.ctd")
    inference = importlib.import_module("modules.ai_backend.detection.textdetector.ctd.inference")
    textmask = importlib.import_module("modules.ai_backend.detection.textdetector.ctd.textmask")
    td_utlis = importlib.import_module("modules.ai_backend.detection.textdetector.td_utlis")

    image_rgb = case.scene.image
    src_h, src_w = image_rgb.shape[:2]
    png_bytes = encode_png(image_rgb)
    image_bgr = cv2.imdecode(np.frombuffer(png_bytes, dtype=np.uint8), cv2.IMREAD_COLOR)

    # The real letterbox decides the map size the network would return.
    _, ratio, (pad_w, pad_h) = td_utlis.letterbox(image_bgr, new_shape=case.detect_size, auto=False, stride=64)
    side = case.detect_size
    map_w, map_h = side - pad_w, side - pad_h
    sx, sy = map_w / src_w, map_h / src_h

    shrink_quads = [(scale_quad(line_quad(line, line.map_shrink), sx, sy), line.map_peak) for line in case.scene.lines]
    shrink_quads += [(scale_quad(np.array(q, dtype=np.float64), sx, sy), peak) for q, peak in case.extra_blobs]
    shrink_u8 = u8_from_prob(soft_quads_map((map_w, map_h), shrink_quads, sigma=1.0))

    strokes = case.scene.strokes.astype(np.float64) / 255.0
    if (map_w, map_h) != (src_w, src_h):
        strokes = cv2.resize(strokes, (map_w, map_h), interpolation=cv2.INTER_AREA)
    seg = cv2.GaussianBlur(strokes, (0, 0), 0.8) * 0.97
    if case.seg_blobs:
        blobs = [(scale_quad(np.array(q, dtype=np.float64), sx, sy), peak) for q, peak in case.seg_blobs]
        seg = np.maximum(seg, soft_quads_map((map_w, map_h), blobs, sigma=1.5))
    seg_u8 = u8_from_prob(np.clip(seg, 0.0, 1.0))

    def fake_net(img_in: Any) -> tuple[Any, Any, Any]:
        if tuple(img_in.shape) != (1, 3, side, side):
            raise FixtureError(f"CTD fake net got input {tuple(img_in.shape)}, expected (1,3,{side},{side})")
        seg_full = np.zeros((side, side), dtype=np.float32)
        seg_full[:map_h, :map_w] = prob_from_u8(seg_u8)
        lines_full = np.zeros((2, side, side), dtype=np.float32)
        lines_full[0, :map_h, :map_w] = prob_from_u8(shrink_u8)
        # A YOLO head with zero confidence: NMS keeps nothing, as upstream drops it anyway.
        blk = torch.zeros((1, 1, 7), dtype=torch.float32)
        return blk, torch.from_numpy(seg_full)[None, None], torch.from_numpy(lines_full)[None]

    recorder = CtdRecorder()

    class FakeNetTextDetector(inference.TextDetector):
        """The real TextDetector with the network replaced by `fake_net`."""

        def load_model(self, model_path: Any) -> None:
            self.net = fake_net
            self.backend = "torch"

        def __call__(self, img: Any, *args: Any, **kwargs: Any) -> Any:
            mask, mask_refined, blk_list = super().__call__(img, *args, **kwargs)
            recorder.seg_source = np.array(mask, copy=True)
            return mask, mask_refined, blk_list

    detector = FakeNetTextDetector("fixture-fake-net", detect_size=side, device="cpu")
    seg_rep = detector.seg_rep

    orig_mini, orig_score, orig_unclip = seg_rep.get_mini_boxes, seg_rep.box_score_fast, seg_rep.unclip

    def rec_mini(contour: Any) -> Any:
        box, sside = orig_mini(contour)
        recorder.db_events.append(("mini", ([[float(p[0]), float(p[1])] for p in box], float(sside))))
        return box, sside

    def rec_score(bitmap: Any, box: Any) -> Any:
        score = orig_score(bitmap, box)
        recorder.db_events.append(("score", float(score)))
        return score

    def rec_unclip(box: Any, unclip_ratio: float = 1.5) -> Any:
        expanded = orig_unclip(box, unclip_ratio=unclip_ratio)
        recorder.db_events.append(("unclip", np.asarray(expanded).reshape(-1, 2).tolist()))
        return expanded

    seg_rep.get_mini_boxes, seg_rep.box_score_fast, seg_rep.unclip = rec_mini, rec_score, rec_unclip

    orig_enlarge = textmask.enlarge_window
    orig_topk_list = textmask.get_topk_masklist
    orig_topk_color = textmask.get_topk_color
    orig_otsu_list = textmask.get_otsuthresh_masklist
    orig_minxor = textmask.minxor_thresh
    orig_merge = textmask.merge_mask_list

    def rec_enlarge(rect: Any, im_w: int, im_h: int, *args: Any, **kwargs: Any) -> Any:
        window = orig_enlarge(rect, im_w, im_h, *args, **kwargs)
        recorder.windows.append({"block_xyxy": [int(v) for v in rect], "window_xyxy": [int(v) for v in window], "topk": {}, "otsu": {}, "candidates_in_merge_order": []})
        return window

    def rec_topk_list(im_grey: Any, pred_mask: Any) -> Any:
        recorder.phase = "topk"
        recorder.current()["topk"]["minxor"] = []
        result = orig_topk_list(im_grey, pred_mask)
        recorder.phase = None
        return result

    def rec_topk_color(color_list: Any, bins: Any, k: int = 3, color_var: int = 10, bin_tol: float = 0.001) -> Any:
        colors = orig_topk_color(color_list, bins, k=k, color_var=color_var, bin_tol=bin_tol)
        stable = stable_topk_colors(np.asarray(color_list), np.asarray(bins), k, color_var, bin_tol)
        record = recorder.current()["topk"]
        record["colors"] = [float(c) for c in colors]
        record["inrange_bounds"] = [[float(min(c + 30, 255) - 60), float(min(c + 30, 255))] for c in colors]
        record["histogram_first_last_edge"] = [float(np.asarray(color_list)[0]), float(np.asarray(color_list)[-1])]
        record["histogram_sample_count"] = int(np.sum(bins))
        # numpy's default argsort is not stable, so tied bin counts may come out in any
        # order. The SET of colours must not depend on it; the order only matters when two
        # candidates tie on their xor sum, which `rec_merge` rejects for flagged blocks.
        if sorted(stable) != sorted(record["colors"]):
            raise FixtureError(f"CTD case {case.name}: top-k colour set depends on argsort tie order ({colors} vs stable {stable})")
        record["stable_sort_order_differs"] = stable != record["colors"]
        return colors

    def rec_otsu_list(img: Any, pred_mask: Any, per_channel: bool = False) -> Any:
        recorder.phase = "otsu"
        record = recorder.current()["otsu"]
        record["minxor"] = []
        record["threshold_per_channel_bgr"] = [float(cv2.threshold(np.ascontiguousarray(img[..., c]), 1, 255, cv2.THRESH_OTSU + cv2.THRESH_BINARY)[0]) for c in range(3)]
        result = orig_otsu_list(img, pred_mask, per_channel=per_channel)
        sums = [entry["xor_sum"] for entry in record["minxor"]]
        record["chosen_channel_bgr"] = int(sums.index(min(sums)))
        recorder.phase = None
        return result

    def rec_minxor(threshed: Any, mask: Any, dilate: bool = False) -> Any:
        chosen, xor_sum = orig_minxor(threshed, mask, dilate=dilate)
        if recorder.phase is None:
            raise FixtureError("minxor_thresh called outside a recorded phase")
        recorder.current()[recorder.phase]["minxor"].append({"xor_sum": int(xor_sum), "inverted": chosen is not threshed})
        return chosen, xor_sum

    def rec_merge(mask_list: Any, pred_mask: Any, *args: Any, **kwargs: Any) -> Any:
        # Python's list.sort is stable: equal xor sums keep the top-k-then-Otsu order.
        sums = sorted(int(entry[1]) for entry in mask_list)
        if recorder.current()["topk"].get("stable_sort_order_differs") and len(set(sums)) != len(sums):
            raise FixtureError(f"CTD case {case.name}: candidate order depends on argsort ties and xor sums tie")
        recorder.current()["candidates_in_merge_order"] = sums
        return orig_merge(mask_list, pred_mask, *args, **kwargs)

    service = object.__new__(ctd_service_mod.CtdTextDetectorService)
    service._cv2 = None
    # Q7(a): the Rust port owns the only dilation, so the reference is recorded without
    # the service's ellipse dilation ("mask dilate size" 0); font params are no-ops.
    params = {"mask dilate size": 0, "font size multiplier": 1.0, "font size max": -1.0, "font size min": -1.0}

    with contextlib.ExitStack() as stack:
        stack.enter_context(mock.patch.object(textmask, "enlarge_window", rec_enlarge))
        stack.enter_context(mock.patch.object(textmask, "get_topk_masklist", rec_topk_list))
        stack.enter_context(mock.patch.object(textmask, "get_topk_color", rec_topk_color))
        stack.enter_context(mock.patch.object(textmask, "get_otsuthresh_masklist", rec_otsu_list))
        stack.enter_context(mock.patch.object(textmask, "minxor_thresh", rec_minxor))
        stack.enter_context(mock.patch.object(textmask, "merge_mask_list", rec_merge))
        stack.enter_context(mock.patch.object(cv2, "connectedComponentsWithStats", cc_check.wrap(cv2.connectedComponentsWithStats)))
        payload = service._detect_from_encoded_image_bytes(png_bytes, detector, params)

    if recorder.seg_source is None:
        raise FixtureError(f"CTD case {case.name}: seg mask was not captured")
    mask = cv2.imdecode(np.frombuffer(payload["mask_png"], dtype=np.uint8), cv2.IMREAD_UNCHANGED)
    if mask is None or mask.shape != (src_h, src_w):
        raise FixtureError(f"CTD case {case.name}: unexpected mask {None if mask is None else mask.shape}")

    candidates = ctd_db_candidates(recorder.db_events)
    for candidate in candidates:
        candidate["kept_score_gt_0_6"] = bool(candidate.get("score", 0.0) > 0.6)
        if "score" in candidate and abs(candidate["score"] - 0.6) < SCORE_MARGIN:
            raise FixtureError(f"CTD case {case.name}: DB score {candidate['score']} is within {SCORE_MARGIN} of 0.6")
    kept = [c for c in candidates if c["kept_score_gt_0_6"]]
    if not any(not c["kept_score_gt_0_6"] and not c["skipped_sside_lt_2"] for c in candidates):
        raise FixtureError(f"CTD case {case.name}: no candidate exercises the score>0.6 filter")
    if not kept:
        raise FixtureError(f"CTD case {case.name}: no block survived")

    write_png(out_dir / "input.png", image_rgb)
    write_png(out_dir / "seg.png", seg_u8)
    write_png(out_dir / "shrink.png", shrink_u8)
    write_png(out_dir / "seg_source.png", recorder.seg_source.astype(np.uint8))
    write_png(out_dir / "expected_mask.png", mask)
    case_json = {
        "engine": "ctd",
        "case": case.name,
        "description": case.description,
        "source_size": [src_w, src_h],
        "map_size": [map_w, map_h],
        "files": {"input": "input.png", "seg": "seg.png", "shrink": "shrink.png", "seg_source": "seg_source.png", "expected_mask": "expected_mask.png"},
        "params": {
            "detect_size": side,
            "letterbox_ratio": float(ratio[0]),
            "letterbox_pad_right_bottom": [int(pad_w), int(pad_h)],
            "db_thresh": float(seg_rep.thresh),
            "db_unclip_ratio": float(seg_rep.unclip_ratio),
            "db_max_candidates": int(seg_rep.max_candidates),
            "db_skip_sside_lt": 2,
            "score_filter_gt": 0.6,
            "mask_binarize_gt": 30,
            "mask_dilate_size": 0,
        },
        "expected": {"blocks": payload["blocks"]},
        "intermediate": {"db_candidates": candidates, "refine_blocks": recorder.windows},
    }
    write_json(out_dir / "case.json", case_json)
    return {"engine": "ctd", "case": case.name, "blocks": len(payload["blocks"])}


def ctd_cases() -> list[CtdCase]:
    """The three CTD cases: unscaled dark-on-light, upscaled light-on-dark, downscaled colour."""
    dark = (20, 20, 25)
    page = np.full((256, 192, 3), 245, dtype=np.uint8)
    page[..., 2] = 238
    dark_lines = [
        TextLine((96, 70), 0.0, 7, 9, 13, 13, dark),
        TextLine((96, 92), 0.0, 6, 9, 13, 13, dark),
        TextLine((90, 150), 15.0, 6, 10, 14, 14, dark),
        TextLine((168, 205), 90.0, 4, 9, 12, 13, dark),
        # A faint line the network is unsure about: its DB score stays below 0.6.
        TextLine((70, 225), 0.0, 4, 8, 11, 12, (90, 90, 95), thickness=1, map_peak=0.5),
    ]
    dark_case = CtdCase(
        name="dark_on_light",
        description="Dark glyphs on light paper; detect_size equals the long side (no resize, right padding). Two close lines (merged refine windows), a 15-degree line, a vertical line, a low-score line, a 1-px speck.",
        scene=render_scene(page, dark_lines, seed=11),
        detect_size=256,
        extra_blobs=((((20.0, 20.0), (22.0, 20.0), (22.0, 20.6), (20.0, 20.6)), 0.9),),
    )

    night = gradient_background(160, 224, (22, 24, 34), (40, 30, 52), 20.0)
    light_lines = [
        TextLine((80, 50), 0.0, 6, 8, 12, 12, (240, 240, 235)),
        TextLine((80, 70), 0.0, 5, 8, 12, 12, (240, 240, 235)),
        TextLine((70, 130), -10.0, 5, 9, 13, 13, (200, 200, 120)),
        TextLine((120, 185), 0.0, 3, 8, 11, 12, (150, 150, 160), thickness=1, map_peak=0.48),
    ]
    light_case = CtdCase(
        name="light_on_dark",
        description="Light glyphs on a dark gradient; detect_size 320 > long side 224, so the letterbox UPSCALES (maps 229x320) and DB quads and the seg mask are mapped back to source px. A false-positive seg blob without a block.",
        scene=render_scene(night, light_lines, seed=23),
        detect_size=320,
        seg_blobs=((((10.0, 200.0), (40.0, 200.0), (40.0, 215.0), (10.0, 215.0)), 0.8),),
    )

    colour = gradient_background(224, 288, (40, 120, 200), (230, 180, 60), 30.0)
    colour_lines = [
        TextLine((112, 70), 0.0, 6, 9, 13, 13, (25, 25, 30)),
        TextLine((112, 92), 0.0, 5, 9, 13, 13, (25, 25, 30)),
        TextLine((70, 190), 0.0, 5, 10, 15, 15, (200, 30, 30), thickness=3),
        TextLine((165, 245), 8.0, 4, 9, 13, 13, (20, 40, 160)),
        TextLine((180, 160), 0.0, 3, 8, 11, 12, (240, 240, 240), thickness=1, map_peak=0.5),
    ]
    colour_case = CtdCase(
        name="color_bg",
        description="Coloured gradient page with a white bubble (dark text), red text and blue text on the gradient; detect_size 256 < long side 288, so the letterbox DOWNSCALES (maps 199x256).",
        scene=render_scene(colour, colour_lines, seed=38, extra=white_bubble((112, 81), (70, 34))),
        detect_size=256,
    )
    return [dark_case, light_case, colour_case]


# ---------------------------------------------------------------------------
# Paddle
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class PaddleCase:
    """One Paddle fixture: scene plus extra probability blobs (source px, peak)."""

    name: str
    description: str
    scene: Scene
    extra_blobs: tuple[tuple[tuple[tuple[float, float], ...], float], ...] = ()


def classify_glyph_branch(roi_bgr: np.ndarray, poly_mask: np.ndarray) -> dict[str, Any]:
    """Annotate which branch `paddle._extract_text_mask_in_roi` takes (mirrors its conditions)."""
    import cv2

    h, w = roi_bgr.shape[:2]
    hsv = cv2.cvtColor(roi_bgr, cv2.COLOR_BGR2HSV)
    sat = hsv[:, :, 1]
    mean_sat = float(cv2.mean(sat, mask=poly_mask)[0])
    info: dict[str, Any] = {"mean_sat": mean_sat}
    if mean_sat > 20:
        _, sat_bin = cv2.threshold(sat, 0, 255, cv2.THRESH_BINARY + cv2.THRESH_OTSU)
        fill = float(cv2.countNonZero(cv2.bitwise_and(sat_bin, poly_mask))) / (h * w)
        info["sat_fill"] = fill
        if 0.01 < fill < 0.85:
            info["branch"] = "saturation"
            return info
    gray = cv2.cvtColor(roi_bgr, cv2.COLOR_BGR2GRAY)
    _, dark_bin = cv2.threshold(gray, 0, 255, cv2.THRESH_BINARY_INV + cv2.THRESH_OTSU)
    fill_dark = float(cv2.countNonZero(cv2.bitwise_and(dark_bin, poly_mask))) / (h * w)
    info["dark_fill"] = fill_dark
    info["branch"] = "dark" if 0.01 < fill_dark < 0.85 else "light"
    return info


def check_glyph_margins(case_name: str, info: dict[str, Any]) -> None:
    """Fail when a glyph-mask decision sits too close to its boundary."""
    if abs(info["mean_sat"] - 20.0) < GLYPH_SAT_MARGIN:
        raise FixtureError(f"Paddle case {case_name}: mean saturation {info['mean_sat']} too close to 20")
    for key in ("sat_fill", "dark_fill"):
        if key in info and min(abs(info[key] - 0.01), abs(info[key] - 0.85)) < GLYPH_FILL_MARGIN:
            raise FixtureError(f"Paddle case {case_name}: {key} {info[key]} too close to a bound")


def run_paddle_case(case: PaddleCase, out_dir: Path, cc_check: ComponentOrderCheck) -> dict[str, Any]:
    """Generate one Paddle fixture directory and return its manifest entry."""
    import cv2

    paddle_mod = importlib.import_module("modules.ai_backend.detection.paddle")
    paddle_onnx = importlib.import_module("modules.ai_backend.engines.paddle_onnx")

    image_rgb = case.scene.image
    src_h, src_w = image_rgb.shape[:2]
    png_bytes = encode_png(image_rgb)
    image_bgr = cv2.imdecode(np.frombuffer(png_bytes, dtype=np.uint8), cv2.IMREAD_COLOR)
    det_cfg = paddle_onnx.parse_det_config(None)
    # The real resize rule decides the map size the network would return.
    resized = paddle_onnx.resize_image_for_det(image_bgr, det_cfg.resize_long, det_cfg.max_stride)
    map_h, map_w = resized.shape[:2]
    sx, sy = map_w / src_w, map_h / src_h

    quads = [(scale_quad(line_quad(line, line.map_shrink), sx, sy), line.map_peak) for line in case.scene.lines]
    quads += [(scale_quad(np.array(q, dtype=np.float64), sx, sy), peak) for q, peak in case.extra_blobs]
    prob_u8 = u8_from_prob(soft_quads_map((map_w, map_h), quads, sigma=1.0))

    post = paddle_onnx.DBPostProcess(thresh=det_cfg.thresh, box_thresh=det_cfg.box_thresh, max_candidates=det_cfg.max_candidates, unclip_ratio=det_cfg.unclip_ratio)
    orig_score, orig_mini = post._box_score_fast, post._get_mini_boxes
    # One record per contour, folded from the call sequence of `_boxes_from_bitmap`:
    # mini box (pre) -> [min_size skip] -> score -> [box_thresh skip] -> unclip -> mini box (post).
    candidates: list[dict[str, Any]] = []
    awaiting_post = False

    def rec_mini(contour: Any) -> Any:
        nonlocal awaiting_post
        box, short_side = orig_mini(contour)
        points = [[float(p[0]), float(p[1])] for p in box]
        if awaiting_post:
            candidates[-1].update({"box_map_px": points, "short_side_post": float(short_side), "skipped_post_lt_min_size_plus_2": short_side < post.min_size + 2})
            awaiting_post = False
        else:
            candidates.append({"mini_box_map_px": points, "short_side_pre": float(short_side), "skipped_lt_min_size": short_side < post.min_size})
        return box, short_side

    def rec_score(bitmap: Any, box: Any) -> float:
        nonlocal awaiting_post
        score = orig_score(bitmap, box)
        candidates[-1]["score"] = float(score)
        candidates[-1]["skipped_lt_box_thresh"] = score < post.box_thresh
        awaiting_post = score >= post.box_thresh
        return score

    post._box_score_fast, post._get_mini_boxes = rec_score, rec_mini

    class FakeRuntime:
        """Stands in for PaddleOnnxRuntime: the forward pass is the fixture map."""

        def detect(self, image: np.ndarray, settings: Any) -> dict[str, Any]:
            if image.shape[:2] != (src_h, src_w):
                raise FixtureError("Paddle fake runtime got an unexpected image")
            boxes, scores = post.process_single(prob_from_u8(prob_u8)[None], src_h, src_w)
            return {"boxes": boxes, "scores": scores}

    branches: list[dict[str, Any]] = []
    orig_extract = paddle_mod._extract_text_mask_in_roi

    def rec_extract(roi_bgr: np.ndarray, poly_mask: np.ndarray) -> np.ndarray:
        info = classify_glyph_branch(roi_bgr, poly_mask)
        check_glyph_margins(case.name, info)
        branches.append(info)
        return orig_extract(roi_bgr, poly_mask)

    service = object.__new__(paddle_mod.PaddleTextDetectorService)
    service._runtime = FakeRuntime()
    with contextlib.ExitStack() as stack:
        # Provider settings only label the payload; never read the machine's user_config.
        stack.enter_context(mock.patch.object(paddle_mod, "resolve_provider_settings", lambda _cfg: SimpleNamespace(provider="CPUExecutionProvider", device_id="0")))
        stack.enter_context(mock.patch.object(paddle_mod, "_extract_text_mask_in_roi", rec_extract))
        stack.enter_context(mock.patch.object(cv2, "connectedComponentsWithStats", cc_check.wrap(cv2.connectedComponentsWithStats)))
        payload = service._detect_from_encoded_bytes(png_bytes)

    for candidate in candidates:
        if "score" in candidate and abs(candidate["score"] - det_cfg.box_thresh) < SCORE_MARGIN:
            raise FixtureError(f"Paddle case {case.name}: DB score {candidate['score']} within {SCORE_MARGIN} of box_thresh")
    if not any(candidate.get("skipped_lt_box_thresh") for candidate in candidates):
        raise FixtureError(f"Paddle case {case.name}: no candidate exercises box_thresh")
    mask = cv2.imdecode(np.frombuffer(payload["mask_png"], dtype=np.uint8), cv2.IMREAD_UNCHANGED)
    if mask is None or mask.shape != (src_h, src_w):
        raise FixtureError(f"Paddle case {case.name}: unexpected mask")
    for index, info in enumerate(branches):
        info["poly_index"] = index

    write_png(out_dir / "input.png", image_rgb)
    write_png(out_dir / "prob.png", prob_u8)
    write_png(out_dir / "expected_mask.png", mask)
    case_json = {
        "engine": "paddle",
        "case": case.name,
        "description": case.description,
        "source_size": [src_w, src_h],
        "map_size": [map_w, map_h],
        "files": {"input": "input.png", "prob": "prob.png", "expected_mask": "expected_mask.png"},
        "params": {
            "db_thresh": det_cfg.thresh,
            "db_box_thresh": det_cfg.box_thresh,
            "db_unclip_ratio": det_cfg.unclip_ratio,
            "db_max_candidates": det_cfg.max_candidates,
            "db_min_size": 3,
        },
        "expected": {"blocks": payload["blocks"], "polys": payload["polys"]},
        "intermediate": {"db_candidates": candidates, "glyph_branches": branches},
    }
    write_json(out_dir / "case.json", case_json)
    return {"engine": "paddle", "case": case.name, "blocks": len(payload["blocks"])}


def paddle_cases() -> list[PaddleCase]:
    """The three Paddle cases: dark text, saturated colour text, light text on dark."""
    page = np.full((300, 230, 3), 248, dtype=np.uint8)
    dark = (25, 25, 30)
    dark_case = PaddleCase(
        name="dark_on_light",
        description="Dark text on white; map 224x288 for a 230x300 source (non-uniform map->source scale). A low-score blob and a 2-px sliver exercise box_thresh and min_size.",
        scene=render_scene(page, [
            TextLine((115, 60), 0.0, 8, 10, 14, 14, dark),
            TextLine((115, 84), 0.0, 6, 10, 14, 14, dark),
            TextLine((100, 170), 12.0, 6, 10, 14, 14, dark),
            TextLine((115, 250), 0.0, 4, 9, 12, 13, (120, 120, 125), thickness=1, map_peak=0.5),
        ], seed=101),
        extra_blobs=((((200.0, 20.0), (220.0, 20.0), (220.0, 21.5), (200.0, 21.5)), 0.95),),
    )

    pale = gradient_background(250, 200, (250, 240, 215), (235, 225, 250), 10.0)
    red = (210, 30, 40)
    sat_case = PaddleCase(
        name="saturated_text",
        description="Saturated red and green text on a pale page (saturation Otsu branch); map 256x192 for a 250x200 source.",
        scene=render_scene(pale, [
            TextLine((125, 50), 0.0, 9, 10, 14, 14, red, thickness=3),
            TextLine((125, 110), -6.0, 7, 10, 14, 14, (20, 150, 40), thickness=3),
            TextLine((125, 165), 0.0, 4, 9, 12, 13, (200, 120, 120), thickness=1, map_peak=0.45),
        ], seed=202),
    )

    night = gradient_background(180, 260, (18, 20, 28), (34, 30, 44), 16.0)
    light_case = PaddleCase(
        name="light_on_dark",
        description="Thin light text on a dark gradient (dark/light Otsu cascade); map 192x256 for a 180x260 source.",
        scene=render_scene(night, [
            TextLine((90, 60), 0.0, 7, 9, 13, 13, (235, 235, 230), thickness=1),
            TextLine((90, 130), 0.0, 6, 9, 13, 13, (235, 235, 230), thickness=2),
            TextLine((90, 210), 0.0, 4, 9, 12, 13, (150, 150, 150), thickness=1, map_peak=0.5),
        ], seed=303),
    )
    return [dark_case, sat_case, light_case]


# ---------------------------------------------------------------------------
# Surya
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SuryaCase:
    """One Surya fixture: scene, heatmap (processor) size and heat-only extra blobs."""

    name: str
    description: str
    scene: Scene
    heat_size: tuple[int, int]
    extra_blobs: tuple[tuple[tuple[tuple[float, float], ...], float], ...] = ()
    floor: float = 0.0
    sigma: float = 1.5


def run_surya_case(case: SuryaCase, out_dir: Path, cc_check: ComponentOrderCheck) -> dict[str, Any]:
    """Generate one Surya fixture directory and return its manifest entry."""
    import cv2
    from surya.detection.heatmap import get_dynamic_thresholds
    from surya.settings import settings

    surya_mod = importlib.import_module("modules.ai_backend.detection.surya")

    image_rgb = case.scene.image
    src_h, src_w = image_rgb.shape[:2]
    heat_w, heat_h = case.heat_size
    if (heat_w, heat_h) == (src_w, src_h):
        raise FixtureError(f"Surya case {case.name}: heatmap size must differ from the source to exercise rescale")
    sx, sy = heat_w / src_w, heat_h / src_h
    quads = [(scale_quad(line_quad(line, line.map_shrink), sx, sy), line.map_peak) for line in case.scene.lines]
    quads += [(scale_quad(np.array(q, dtype=np.float64), sx, sy), peak) for q, peak in case.extra_blobs]
    heat_u8 = u8_from_prob(soft_quads_map((heat_w, heat_h), quads, sigma=case.sigma, floor=case.floor))
    heat = prob_from_u8(heat_u8)

    text_thr_cfg = float(settings.DETECTOR_TEXT_THRESHOLD)
    low_cfg = float(settings.DETECTOR_BLANK_THRESHOLD)
    text_thr, low_text = get_dynamic_thresholds(heat, text_thr_cfg, low_cfg)
    # A threshold equal to a configured constant or a clip bound is computed exactly by any
    # port; only a SCALED threshold carries float-summation error, so only it needs a margin
    # to the u8 levels (0.6 itself IS level 153, compared with strict `<` in float32).
    exact = {float(np.float32(v)) for v in (text_thr_cfg, low_cfg, 0.1, 0.15, 0.6, 0.8)}
    for name, value in (("text_threshold", text_thr), ("low_text", low_text)):
        if float(value) not in exact and u8_level_margin(float(value)) < THRESHOLD_LEVEL_MARGIN:
            raise FixtureError(f"Surya case {case.name}: {name} {float(value)} is within {THRESHOLD_LEVEL_MARGIN} of a u8 level")

    with mock.patch.object(cv2, "connectedComponentsWithStats", cc_check.wrap(cv2.connectedComponentsWithStats)):
        boxes, confidences, proc_mask, stats = surya_mod._extract_mask_and_boxes(cv2=cv2, linemap=heat, text_threshold=text_thr_cfg, low_text=low_cfg)

    components: list[dict[str, Any]] = []
    count, labels, cc_stats, _ = cv2.connectedComponentsWithStats((heat > low_text).astype(np.uint8), connectivity=4)
    for label in range(1, count):
        x, y, w, h, area = (int(v) for v in cc_stats[label])
        line_max = float(np.max(heat[labels == label]))
        outcome = "area_lt_10" if area < 10 else ("max_lt_text_threshold" if line_max < text_thr else "kept")
        if outcome != "area_lt_10" and abs(line_max - float(text_thr)) < 1e-4 and float(text_thr) not in exact:
            raise FixtureError(f"Surya case {case.name}: component max {line_max} too close to the scaled text threshold")
        components.append({"label": label, "bbox_xywh": [x, y, w, h], "area": area, "max": line_max, "outcome": outcome})

    class FakePredictor:
        """Yields the fixture heatmap as the predictor's stitched channel-0 output."""

        def batch_detection(self, images: list[Any], batch_size: int, static_cache: bool) -> Iterator[Any]:
            if len(images) != 1 or images[0].size != (src_w, src_h):
                raise FixtureError("Surya fake predictor got an unexpected image")
            yield [np.stack([heat, np.zeros_like(heat)])], [images[0].size]

    service = object.__new__(surya_mod.SuryaTextDetectorService)
    service._device = "cpu"
    with mock.patch.object(cv2, "connectedComponentsWithStats", cc_check.wrap(cv2.connectedComponentsWithStats)):
        payload = service._detect_with_predictor(encode_png(image_rgb), FakePredictor())

    mask = cv2.imdecode(np.frombuffer(payload["mask_png"], dtype=np.uint8), cv2.IMREAD_UNCHANGED)
    if mask is None or mask.shape != (src_h, src_w):
        raise FixtureError(f"Surya case {case.name}: unexpected mask")
    if not np.array_equal(mask, cv2.resize(proc_mask, (src_w, src_h), interpolation=cv2.INTER_NEAREST)):
        raise FixtureError(f"Surya case {case.name}: service mask differs from the recorded processor mask")

    write_png(out_dir / "input.png", image_rgb)
    write_png(out_dir / "heat.png", heat_u8)
    write_png(out_dir / "proc_mask.png", proc_mask)
    write_png(out_dir / "expected_mask.png", mask)
    case_json = {
        "engine": "surya",
        "case": case.name,
        "description": case.description,
        "source_size": [src_w, src_h],
        "map_size": [heat_w, heat_h],
        "files": {"input": "input.png", "heat": "heat.png", "proc_mask": "proc_mask.png", "expected_mask": "expected_mask.png"},
        "params": {
            "text_threshold": text_thr_cfg,
            "low_text": low_cfg,
            "typical_top10_avg": 0.7,
            "y_expand_margin": float(settings.DETECTOR_BOX_Y_EXPAND_MARGIN),
            "min_component_area": 10,
        },
        "expected": {"blocks": payload["blocks"], "lines": payload["lines"]},
        "intermediate": {
            "dynamic_text_threshold_f32": float(text_thr),
            "dynamic_low_text_f32": float(low_text),
            "label_count": stats["label_count"],
            "components": components,
            "max_confidence": stats["max_confidence"],
            "proc_boxes": [[[float(x), float(y)] for x, y in np.asarray(b).tolist()] for b in boxes],
            "proc_confidences": [float(c) for c in confidences],
        },
    }
    write_json(out_dir / "case.json", case_json)
    return {"engine": "surya", "case": case.name, "blocks": len(payload["blocks"])}


def ring_quads(x0: float, y0: float, x1: float, y1: float, t: float) -> list[tuple[tuple[float, float], ...]]:
    """Four bars forming a rectangular frame (one connected component around a hole)."""
    return [
        ((x0, y0), (x1, y0), (x1, y0 + t), (x0, y0 + t)),
        ((x0, y1 - t), (x1, y1 - t), (x1, y1), (x0, y1)),
        ((x0, y0), (x0 + t, y0), (x0 + t, y1), (x0, y1)),
        ((x1 - t, y0), (x1, y0), (x1, y1), (x1 - t, y1)),
    ]


def surya_cases() -> list[SuryaCase]:
    """The three Surya cases: both axes stretched, width only, low contrast."""
    page = np.full((180, 240, 3), 246, dtype=np.uint8)
    dark = (25, 25, 30)
    stretch = SuryaCase(
        name="stretch_both",
        description="Strong heatmap 256x256 for a 240x180 source (both axes rescaled); dense text so the top-10% mean saturates the dynamic thresholds at their configured values. Includes a 20-degree line and a tall vertical line (no y-expand).",
        scene=render_scene(page, [
            TextLine((120, 30), 0.0, 14, 11, 15, 14, dark, map_peak=0.97),
            TextLine((120, 55), 0.0, 13, 11, 15, 14, dark, map_peak=0.97),
            TextLine((110, 110), 20.0, 9, 11, 15, 14, dark, map_peak=0.95),
            TextLine((220, 120), 90.0, 6, 9, 12, 12, dark, map_peak=0.95),
        ], seed=404),
        heat_size=(256, 256),
    )

    tall = np.full((320, 200, 3), 244, dtype=np.uint8)
    # Heat px per source px of this case (heat 240x320 over a 200x320 source).
    to_source = np.array([200.0 / 240.0, 1.0])
    angle = np.deg2rad(30.0)
    rotation = np.array([[np.cos(angle), -np.sin(angle)], [np.sin(angle), np.cos(angle)]])
    # A square rotated by 30 degrees IN HEAT SPACE: minAreaRect sees a near-square, so the
    # axis-aligned box rule replaces it.
    diamond = (np.array([[-10.0, -10.0], [10.0, -10.0], [10.0, 10.0], [-10.0, 10.0]]) @ rotation.T + np.array([180.0, 150.0])) * to_source
    # A 3x3 heat-px dot: after the blur it is a component smaller than 10 px.
    dot = np.array([[100.0, 300.0], [103.0, 300.0], [103.0, 303.0], [100.0, 303.0]]) * to_source
    blobs = tuple(ring_quads(30.0, 200.0, 110.0, 270.0, 6.0)) + (
        # Inside the frame: its box is contained in the frame's box -> clean_boxes drops it.
        ((60.0, 225.0), (80.0, 225.0), (80.0, 245.0), (60.0, 245.0)),
        tuple(map(tuple, diamond.tolist())),
    )
    width_only = SuryaCase(
        name="width_only",
        description="Heatmap 240x320 for a 200x320 source (only x rescaled, the tall-page chunk geometry). A frame-shaped component containing a separate blob (clean_boxes), a square rotated 30 degrees in heat space (axis-box rule), a weak line below text_threshold, a small dot kept, and a component below area 10.",
        scene=render_scene(tall, [
            TextLine((100, 40), 0.0, 12, 11, 15, 14, dark, map_peak=0.96),
            TextLine((100, 65), 0.0, 10, 11, 15, 14, dark, map_peak=0.96),
            TextLine((80, 120), 0.0, 5, 9, 12, 12, (140, 140, 140), thickness=1, map_peak=0.45),
            TextLine((170, 300), 0.0, 1, 3, 3, 3, dark, thickness=1, map_peak=0.9, map_shrink=0.0, draw_text=False),
        ], seed=505),
        heat_size=(240, 320),
        extra_blobs=tuple((q, 0.92) for q in blobs) + ((tuple(map(tuple, dot.tolist())), 0.65),),
    )

    grey = gradient_background(180, 240, (200, 200, 205), (225, 220, 210), 12.0)
    low = SuryaCase(
        name="low_contrast",
        description="Weak heatmap (peaks 0.42-0.55 over a 0.2 floor) 256x200 for a 180x240 source: the top-10% mean is below 0.7, so both dynamic thresholds scale down without clipping.",
        scene=render_scene(grey, [
            TextLine((90, 40), 0.0, 8, 10, 14, 14, (60, 60, 70), map_peak=0.55),
            TextLine((90, 62), 0.0, 7, 10, 14, 14, (60, 60, 70), map_peak=0.55),
            TextLine((90, 130), -12.0, 7, 10, 14, 14, (80, 70, 60), map_peak=0.5),
            TextLine((90, 200), 0.0, 5, 10, 14, 14, (90, 90, 95), map_peak=0.42),
        ], seed=606),
        heat_size=(256, 200),
        floor=0.2,
    )
    return [stretch, width_only, low]


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------


def import_reference_modules() -> None:
    """Import every reference module, or exit(2) with a message naming what is missing."""
    if str(REPO_ROOT) not in sys.path:
        sys.path.insert(0, str(REPO_ROOT))
    for name in REFERENCE_MODULES:
        try:
            importlib.import_module(name)
        except ModuleNotFoundError as exc:
            if exc.name is not None and (exc.name == name or name.startswith(exc.name + ".")):
                log.error(
                    "Reference module %s is absent, so the Python postprocess these fixtures are taken from no longer exists in this checkout.\n"
                    "Keep the committed fixtures as they are, or check out a revision that still has the Python postprocess.",
                    name,
                )
            else:
                log.error("Reference module %s needs %s, which is not installed in this interpreter (%s).\nRun the script with ./venv/bin/python.", name, exc.name, sys.executable)
            raise SystemExit(2) from exc
    try:
        importlib.import_module("surya.detection.heatmap")
    except ModuleNotFoundError as exc:
        log.error("The surya package (%s) is not installed in this interpreter (%s).\nRun the script with ./venv/bin/python.", exc.name, sys.executable)
        raise SystemExit(2) from exc


def package_versions() -> dict[str, str]:
    """Versions of the libraries whose numerics the fixtures depend on."""
    from importlib.metadata import PackageNotFoundError, version

    names = ("numpy", "opencv-python", "opencv-python-headless", "opencv-contrib-python", "pillow", "torch", "pyclipper", "shapely", "surya-ocr")
    found: dict[str, str] = {}
    for name in names:
        try:
            found[name] = version(name)
        except PackageNotFoundError:
            # Only one of the OpenCV distributions is installed; absent names are skipped.
            continue
    import cv2

    found["cv2.__version__"] = cv2.__version__
    found["python"] = sys.version.split()[0]
    return found


def file_hashes(directory: Path) -> dict[str, str]:
    """sha256 of every file in one case directory, keyed by file name."""
    return {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(directory.iterdir()) if path.is_file()}


def check_coverage() -> None:
    """Fail unless the written cases exercise every branch the ports must reproduce."""
    cases = [json.loads(path.read_text(encoding="utf-8")) for path in sorted(FIXTURES_DIR.glob("*/*/case.json"))]
    seen: set[str] = set()
    for case in cases:
        inter = case["intermediate"]
        if case["engine"] == "ctd":
            for cand in inter["db_candidates"]:
                seen.add("ctd:skip_sside" if cand["skipped_sside_lt_2"] else ("ctd:kept" if cand["kept_score_gt_0_6"] else "ctd:score_filtered"))
            for block in inter["refine_blocks"]:
                for phase in ("topk", "otsu"):
                    for entry in block[phase]["minxor"]:
                        seen.add(f"ctd:{phase}_{'inverted' if entry['inverted'] else 'normal'}")
            if case["source_size"] != case["map_size"]:
                seen.add("ctd:scaled")
        elif case["engine"] == "paddle":
            seen.update(f"paddle:{branch['branch']}" for branch in inter["glyph_branches"])
            for cand in inter["db_candidates"]:
                if cand["skipped_lt_min_size"]:
                    seen.add("paddle:skip_min_size")
                elif cand["skipped_lt_box_thresh"]:
                    seen.add("paddle:skip_box_thresh")
        else:
            seen.update(f"surya:{component['outcome']}" for component in inter["components"])
            if inter["dynamic_text_threshold_f32"] != float(np.float32(case["params"]["text_threshold"])):
                seen.add("surya:scaled_thresholds")
    required = {
        "ctd:skip_sside", "ctd:kept", "ctd:score_filtered", "ctd:topk_normal", "ctd:otsu_inverted", "ctd:otsu_normal", "ctd:scaled",
        "paddle:saturation", "paddle:dark", "paddle:light", "paddle:skip_min_size", "paddle:skip_box_thresh",
        "surya:area_lt_10", "surya:max_lt_text_threshold", "surya:kept", "surya:scaled_thresholds",
    }
    missing = sorted(required - seen)
    if missing:
        raise FixtureError(f"fixture cases no longer exercise: {missing}")


def main() -> int:
    """Regenerate all fixtures; return the process exit status."""
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s")
    # The services log every request at INFO; keep the tool's own output readable.
    logging.getLogger("modules").setLevel(logging.WARNING)
    import_reference_modules()

    revision = git_revision()
    if revision["uncommitted_reference_edits"]:
        log.warning("Reference paths have uncommitted edits; the manifest records them: %s", revision["uncommitted_reference_edits"])

    for engine in ENGINE_DIRS:
        target = FIXTURES_DIR / engine
        if target.exists():
            shutil.rmtree(target)

    cc_check = ComponentOrderCheck()
    entries: list[dict[str, Any]] = []
    runs: list[tuple[str, Callable[[Any, Path, ComponentOrderCheck], dict[str, Any]], list[Any]]] = [
        ("ctd", run_ctd_case, ctd_cases()),
        ("paddle", run_paddle_case, paddle_cases()),
        ("surya", run_surya_case, surya_cases()),
    ]
    for engine, runner, cases in runs:
        for case in cases:
            out_dir = FIXTURES_DIR / engine / case.name
            entry = runner(case, out_dir, cc_check)
            entry["files"] = file_hashes(out_dir)
            entries.append(entry)
            log.info("Wrote %s/%s: %d blocks", engine, case.name, entry["blocks"])

    check_coverage()
    from surya.settings import settings

    manifest = {
        "generator": "tools/make_text_detect_fixtures.py",
        "git": revision,
        "versions": package_versions(),
        "surya_settings": {
            "DETECTOR_TEXT_THRESHOLD": float(settings.DETECTOR_TEXT_THRESHOLD),
            "DETECTOR_BLANK_THRESHOLD": float(settings.DETECTOR_BLANK_THRESHOLD),
            "DETECTOR_BOX_Y_EXPAND_MARGIN": float(settings.DETECTOR_BOX_Y_EXPAND_MARGIN),
        },
        "checks": {"connected_components_label_order_verified_calls": {str(k): v for k, v in sorted(cc_check.calls_by_connectivity.items())}},
        "cases": entries,
    }
    write_json(FIXTURES_DIR / "manifest.json", manifest)
    total = sum(path.stat().st_size for path in FIXTURES_DIR.rglob("*") if path.is_file())
    log.info("Fixtures written to %s (%d bytes total)", FIXTURES_DIR, total)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except FixtureError as exc:
        log.error("Fixture generation failed: %s", exc)
        sys.exit(1)
