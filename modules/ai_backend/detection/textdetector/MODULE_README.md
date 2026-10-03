# Module: modules/ai_backend/detection/textdetector

## Purpose
Vendored ComicTextDetector network (BallonTranslator lineage) — the model code behind
`detection/ctd.py`. It is third-party code kept in-tree, not project-authored code: prefer minimal,
clearly marked edits here and keep backend logic in the service layer above. Only the network is
kept; the upstream inference pipeline and post-processing (letterbox, rearrange, DB decoding, mask
refinement) were removed when detection post-processing moved to Rust (`crates/ms-text-detect`).

## Architecture
`detection/ctd.py` imports `TextDetBase` lazily from `ctd/basemodel.py`, constructs it with a
checkpoint path and a device, and calls it with an NCHW float32 batch. `TextDetBase.forward` returns
`(blks, mask, lines)`: the YOLOv5 block head output (unused), the segmentation map `[N, 1, H, W]`
and the DB maps `[N, 2, H, W]` (shrink, threshold). The service keeps `mask` and `lines[:, 0]`.

## Files and submodules
- `__init__.py`, `ctd/__init__.py`: import-free docstrings, so importing `ctd.basemodel` loads
  nothing else.
- `ctd/basemodel.py`: `TextDetBase` (YOLOv5 backbone + UNet segmentation head + DB head) and the
  checkpoint loader `get_base_det_models`.
- `yolov5/`: trimmed YOLOv5 backbone/utilities (`common.py`, `yolo.py`, `yolov5_utils.py`) used by
  `ctd/basemodel.py`.

## Contracts and invariants
- This subtree is vendored. Keep divergence from upstream small and local, and do not reshape its
  module layout — `ctd/` and `yolov5/` are import-path-sensitive.
- Import cost is deliberately deferred: `detection/ctd.py` imports `ctd.basemodel` inside a method
  so Torch loads only when CTD actually runs. Do not add a top-level import of this package anywhere
  in the backend, and keep both `__init__.py` files import-free.
- Input sides must be multiples of 64 (the UNet head halves the resolution six times).
- Weights are Torch weights under `ManhwaStudio_AI_Models/Torch` (`config.TEXT_DETECTOR_DIR`); the
  path is passed in by the service.
- Heavy third-party requirements live only here: `torch`, `torchvision` (NMS helpers in
  `yolov5_utils.py`), `opencv`. Nothing outside `detection/ctd.py` may depend on this package.
- Missing coverage (pre-existing, per CLAUDE.md §16): no tests. Vendored model code is verified
  against upstream and by running detection, not by unit tests in this repo.

## Editing map
- To change the network or checkpoint loading, edit `ctd/basemodel.py` (minimal, marked edits).
- To change anything the service owns (device selection, normalization, output channels), edit
  `detection/ctd.py`; to change detection quality or post-processing, edit `crates/ms-text-detect`.
