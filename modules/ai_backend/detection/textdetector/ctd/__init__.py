"""Vendored ComicTextDetector network (`basemodel.TextDetBase`).

Import-free on purpose: `detection/ctd.py` imports `basemodel` lazily, and the
upstream re-export of the inference pipeline was removed together with that
pipeline (post-processing now lives in Rust, `crates/ms-text-detect`).
"""
