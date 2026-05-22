from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional, Tuple

import cv2
import numpy as np


class Registry:
    """Minimal registry helper used for detector decorators."""

    def __init__(self, name: str):
        self.name = name
        self.module_dict: Dict[str, Callable] = {}

    def register_module(self, name: Optional[str] = None) -> Callable:
        def decorator(cls):
            key = name or getattr(cls, "__name__", str(cls))
            self.module_dict[key] = cls
            return cls

        return decorator


@dataclass
class TextBlock:
    xyxy: List[int]
    lines: List[List[List[int]]] = field(default_factory=list)
    language: str = "unknown"
    vertical: bool = False
    font_size: float = -1.0
    _detected_font_size: float = -1.0
    det_model: Optional[str] = None


class ProjImgTrans:
    """Placeholder for project context; kept for signature compatibility."""

    pass


def letterbox(im: np.ndarray, new_shape=(640, 640), color=(0, 0, 0), auto=False, scaleFill=False, scaleup=True, stride=128):
    """Resize and pad image while meeting stride-multiple constraints."""
    shape = im.shape[:2]  # current shape [height, width]
    if not isinstance(new_shape, tuple):
        new_shape = (new_shape, new_shape)

    r = min(new_shape[0] / shape[0], new_shape[1] / shape[1])
    if not scaleup:
        r = min(r, 1.0)

    ratio = r, r
    new_unpad = int(round(shape[1] * r)), int(round(shape[0] * r))
    dw, dh = new_shape[1] - new_unpad[0], new_shape[0] - new_unpad[1]
    if auto:
        dw, dh = np.mod(dw, stride), np.mod(dh, stride)
    elif scaleFill:
        dw, dh = 0.0, 0.0
        new_unpad = (new_shape[1], new_shape[0])
        ratio = new_shape[1] / shape[1], new_shape[0] / shape[0]

    dh, dw = int(dh), int(dw)

    if shape[::-1] != new_unpad:
        im = cv2.resize(im, new_unpad, interpolation=cv2.INTER_LINEAR)
    im = cv2.copyMakeBorder(im, 0, dh, 0, dw, cv2.BORDER_CONSTANT, value=color)
    return im, ratio, (dw, dh)


def square_pad_resize(img: np.ndarray, tgt_size: int):
    """Pad to square then downscale so the longest side equals tgt_size."""
    h, w = img.shape[:2]
    pad_h, pad_w = 0, 0

    if w < h:
        pad_w = h - w
        w += pad_w
    elif h < w:
        pad_h = w - h
        h += pad_h

    pad_size = tgt_size - h
    if pad_size > 0:
        pad_h += pad_size
        pad_w += pad_size

    if pad_h > 0 or pad_w > 0:
        img = cv2.copyMakeBorder(img, 0, pad_h, 0, pad_w, cv2.BORDER_CONSTANT)

    down_scale_ratio = tgt_size / img.shape[0]
    if down_scale_ratio < 1:
        img = cv2.resize(img, (tgt_size, tgt_size), interpolation=cv2.INTER_AREA)

    return img, down_scale_ratio, pad_h, pad_w


def union_area(bboxa: List[int], bboxb: List[int]) -> int:
    """Intersection area of two axis-aligned boxes."""
    x1 = max(bboxa[0], bboxb[0])
    y1 = max(bboxa[1], bboxb[1])
    x2 = min(bboxa[2], bboxb[2])
    y2 = min(bboxa[3], bboxb[3])
    if y2 < y1 or x2 < x1:
        return 0
    return int((y2 - y1) * (x2 - x1))


def enlarge_window(rect: List[int], im_w: int, im_h: int, ratio: float = 2.5, aspect_ratio: float = 1.0) -> List[int]:
    """
    Expand a bounding box while keeping it inside the image.
    Ported from the original util with simplified math.
    """
    assert ratio > 1.0
    x1, y1, x2, y2 = rect
    w = x2 - x1
    h = y2 - y1

    if w <= 0 or h <= 0:
        return [0, 0, 0, 0]

    coeff = [aspect_ratio, w + h * aspect_ratio, (1 - ratio) * w * h]
    roots = np.roots(coeff)
    roots.sort()
    delta = int(round(roots[-1] / 2))
    delta_w = int(delta * aspect_ratio)
    delta_w = min(x1, im_w - x2, delta_w)
    delta = min(y1, im_h - y2, delta)
    rect = np.array([x1 - delta_w, y1 - delta, x2 + delta_w, y2 + delta], dtype=np.int64)
    rect[::2] = np.clip(rect[::2], 0, im_w)
    rect[1::2] = np.clip(rect[1::2], 0, im_h)
    return rect.tolist()


def group_output(blks, lines, im_w: int, im_h: int, mask=None, sort_blklist=True, canvas=None) -> List[TextBlock]:
    """
    Build simple TextBlock objects from detected lines.
    ComicTextDetector currently calls this with bounding boxes disabled.
    """
    if lines is None:
        return []
    blocks: List[TextBlock] = []
    for line in lines:
        pts = np.asarray(line, dtype=np.int64).reshape(-1, 2)
        x1, y1 = pts.min(axis=0)
        x2, y2 = pts.max(axis=0)
        blk = TextBlock([int(x1), int(y1), int(x2), int(y2)], [pts.tolist()])
        blk.vertical = (y2 - y1) > (x2 - x1)
        blocks.append(blk)
    return blocks

