# Module: modules/ai_backend/ocr/paddle_vl_vendor

## Purpose
Vendored, unmodified PaddleOCR-VL model code. `../paddle_vl.py` builds the processor and the model
from these classes, so the backend never runs `trust_remote_code=True` and never executes Python
downloaded at run time. Third-party code under Apache-2.0 (`LICENSE`); the attribution is in the
repository-root `NOTICE`.

## Provenance
Hugging Face `PaddlePaddle/PaddleOCR-VL-1.6`, commit `c5630abae1d940eafe0697512a0325494b02ab42`,
copied byte for byte (fetched from `resolve/<commit>/<file>`):

| file | sha256 |
|---|---|
| `configuration_paddleocr_vl.py` | `753dd93654c3a9c8c85a3eaee1e3092dd12591b0f2dce0305e1abfb7a41ff160` |
| `modeling_paddleocr_vl.py` | `c5013dff57ca8b87dc1de64d0fd839a44313de09d230a4fb2d08289d2cad5111` |
| `processing_paddleocr_vl.py` | `e29cb1e5f275f2bd3ce051bd5c9983a33894e693b2823a0e13d4c07c8c4f9e13` |
| `image_processing_paddleocr_vl.py` | `a4fa521b9cb16e207f94b7f2d16427771776dfc634420d319fc4916ee58049ec` |
| `LICENSE` | `b8c4d7deccd236af023af1c88c4d4e8f0fc2f41914e0fb23a3ec9678fb5a8456` |

`PaddlePaddle/PaddleOCR-VL-1.5` at `2a4195faa5e7914c12f2fc601d72c81caf8d2da5` ships byte-identical
`.py` files.

## Why one copy serves every variant
The backend loads three checkpoints with this code: `official_1_6`, `official_1_5` and `manga_ja`
(`jzhang533/PaddleOCR-VL-For-Manga` @ `1e8aa5f1dd90cc86fe9137c9c0b26ebde613cfe8`, a full fine-tune
of PaddleOCR-VL v1). manga_ja ships the older pre-rename `Siglip*` code; after renaming, it differs
only in the causal-mask helper, `base_model_prefix` and docstrings, and its image processor only
in `print()` lines. Probes on transformers 4.57.1 with the `paddle_vl.py` shims applied showed:
processor prompt and every tensor equal to each repository's own `AutoProcessor` (4 image sizes);
equal `state_dict` key and shape sets (620 tensors); logits on a 2-layer random-weight copy
bit-identical (max abs diff 0.0); 24-token greedy output identical with `use_cache` on and off.
Real-weight loading of manga_ja through these classes is guarded at run time: `paddle_vl.py`
refuses a load whose loading info reports missing, unexpected or mismatched keys.

## Files
- `configuration_paddleocr_vl.py`, `modeling_paddleocr_vl.py`, `processing_paddleocr_vl.py`,
  `image_processing_paddleocr_vl.py`: the vendored code. Its only relative import is
  `modeling -> .configuration`.
- `LICENSE`: the upstream Apache-2.0 license file at the same commit.
- `__init__.py`: docstring only.

## Contracts and invariants
- **Never edit the four vendored files**, not even to add the project header or to run a
  formatter: `../test_paddle_vl_vendor.py` pins their sha256 (CRLF-normalized, so a Windows
  checkout still verifies). They are the declared exception to the file-header rule.
- Import them only lazily and only after `paddle_vl._ensure_transformers_compat()`:
  `modeling_paddleocr_vl.py` binds `create_causal_mask` and `check_model_inputs` at import time.
- The code is tied to the transformers 4.x API plus those shims (transformers is not pinned by the
  installer).
- `image_processing_paddleocr_vl.py` prints to stdout from `smart_resize` for crops under 28 px;
  accepted as upstream behaviour.

## Editing map
- To move to a newer upstream revision: copy the new files byte for byte, update the table above,
  `../test_paddle_vl_vendor.py` and the root `NOTICE`, re-run the equivalence probes for every
  variant, and check the shims in `../paddle_vl.py`.
