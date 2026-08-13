# Module: modules/ai_backend/detection/textdetector

## Purpose
Vendored and adapted ComicTextDetector implementation (BallonTranslator lineage) — the actual model
code behind `detection/ctd.py`. It is third-party code kept in-tree, not project-authored code:
prefer minimal, clearly marked edits here and keep new backend logic in the service layer above.

## Architecture
`detection/ctd.py` imports `CTDModel` lazily from `ctd/`, constructs it with a checkpoint path,
`detect_size`, `device` and `det_rearrange_max_batches`, and calls it with a BGR image. The model
returns `(mask, mask_refined, blk_list)` and the service uses the refined mask; everything above the
model — parameter clamping,
mask dilation, font-size post-processing, PNG encoding — belongs to the service, not here.

Inside, `ctd/inference.py` is the pipeline: letterbox/rearrange the page, run `TextDetBase`
(a YOLOv5 backbone plus a DB-style segmentation head), decode boxes with NMS from `yolov5/`,
decode the mask with `db_utils.SegDetectorRepresenter`, then refine it in `ctd/textmask.py`.

The `TextDetectorBase` / registry layer in `base.py` + `detector_ctd.py` is the upstream plugin
interface. The backend does not go through it (`detection/ctd.py` drives `CTDModel` directly), but
it is what pins the dependency on `detection/base.py`.

## Files and submodules
- `base.py`: upstream `TextDetectorBase` and the `TEXTDETECTORS` registry. **Imports
  `BaseModule`, `DEFAULT_DEVICE`, `DEVICE_SELECTOR` from `detection/base.py` via `from ..base
  import ...`** — this is the only edge into that file and the reason the two must stay siblings.
- `detector_ctd.py`: upstream registry-registered CTD wrapper. Kept for parity with upstream; the
  backend service does not use it.
- `td_utlis.py`: `TextBlock`, `Registry`, `ProjImgTrans`, letterbox/pad/grouping helpers (upstream
  filename typo preserved).
- `db_utils.py`: DB (Differentiable Binarization) post-processing — `SegDetectorRepresenter` and
  polygon/unclip helpers. Needs `pyclipper` and `shapely`.
- `ctd/`: the model itself. `__init__.py` exports `TextDetector as CTDModel`; `inference.py` is the
  end-to-end pipeline; `basemodel.py` builds `TextDetBase` from the YOLOv5 parts; `textmask.py`
  refines the segmentation mask.
- `yolov5/`: trimmed YOLOv5 backbone/utilities (`common.py`, `yolo.py`, `yolov5_utils.py`) used only
  by `ctd/basemodel.py` and for NMS.

## Contracts and invariants
- This subtree is vendored. Keep divergence from upstream small and local, and do not reshape its
  module layout — `ctd/`, `yolov5/`, and the `..base` edge are all import-path-sensitive.
- Import cost is deliberately deferred: `detection/ctd.py` imports `ctd.CTDModel` inside a method so
  Torch loads only when CTD actually runs. Do not add a top-level import of this package anywhere in
  the backend.
- Checkpoint location comes from the root `config.TEXT_DETECTOR_DIR` (an absolute `config` import,
  independent of this package's depth). Weights are Torch weights under `ManhwaStudio_AI_Models/Torch`.
- Heavy third-party requirements live only here: `torch`, `torchvision`, `einops`, `pyclipper`,
  `shapely`. Nothing outside `detection/ctd.py` may depend on this package.
- Missing coverage (pre-existing, per CLAUDE.md §16): no tests. Vendored model code is verified
  against upstream and by running detection, not by unit tests in this repo.

## Editing map
- To change detection quality, thresholds, or mask refinement, edit `ctd/inference.py` and
  `ctd/textmask.py`.
- To change box decoding/NMS, edit `yolov5/yolov5_utils.py`.
- To change the DB mask decoder, edit `db_utils.py`.
- To change anything the service owns (params, device selection, dilation, output shape), edit
  `detection/ctd.py` instead — do not push backend policy into vendored code.
