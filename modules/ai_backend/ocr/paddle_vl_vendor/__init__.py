"""
Package: modules/ai_backend/ocr/paddle_vl_vendor

Unmodified copy of the PaddleOCR-VL model code from the Hugging Face repository
`PaddlePaddle/PaddleOCR-VL-1.6` at commit
`c5630abae1d940eafe0697512a0325494b02ab42` (Apache-2.0, see `LICENSE` here).
It replaces `trust_remote_code=True`: the backend executes these reviewed,
hash-pinned files instead of code downloaded at run time. Provenance, the
sha256 table and why one copy serves every variant: `MODULE_README.md`.

The four vendored modules carry no project header ON PURPOSE — any edit would
break their sha256 pin (`../test_paddle_vl_vendor.py`). This file re-exports
nothing; `../paddle_vl.py::_load_vendored_classes()` imports the modules lazily,
after `_ensure_transformers_compat()`.
"""
