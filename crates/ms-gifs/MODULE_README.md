# Module: crates/ms-gifs

## Purpose
GUI-free storage and decoding for animated WebP help hints used by ManhwaStudio.
The assets live in this small crate so ordinary edits to the main crate's `src/`
do not recompile roughly 2.9 MB of embedded bytes.

## Architecture
`typing` defines each asset as a `Hint`, combining a stable name with bytes
embedded by `include_bytes!`. The crate root validates the WebP animation layout,
uses `image-webp` to produce fully composited frames, and normalizes RGB output to
RGBA. The normalization path supports future alpha-less assets but is not exercised
by the eight currently shipped alpha-bearing animations. The crate has no dependency
on egui or the `image` crate; GUI consumers perform texture upload separately.

Data flow: `typing::<HINT>` -> `decode(Hint)` -> `image-webp` compositing ->
`Animation { width, height, frames }` -> background-thread GUI consumer.

## Files and submodules
- `src/lib.rs`: public data model, hint accessors, complete inventory, and decoder.
- `src/typing.rs`: stable typing-hint identities and `include_bytes!` declarations.
- `src/error.rs`: typed validation and decoder failures carrying the hint name.
- `assets/`: source animated WebP files embedded into the library artifact.
- `tests/assets_decode.rs`: CI validation of shipped asset structure, pixels, timing,
  animation changes, and stable-name uniqueness.

## Contracts and invariants
- `Hint::name()` is a unique, stable cache key and must not change across runs.
- Assets are embedded with `include_bytes!`, so this crate recompiles when an asset
  changes while unrelated main-crate source edits do not rebuild the asset bytes.
- Successful decoding returns non-zero dimensions and at least one frame.
- Every `Frame::rgba` is non-premultiplied RGBA8 with exactly `width * height * 4`
  bytes, including when the decoder reports RGB source output.
- Frame delays use the millisecond values stored in the WebP animation.
- Decoding is expensive in CPU and memory and must never run on the GUI thread.
- Invalid, static, or empty assets return `GifError`; unit tests lock in the static,
  garbage, and empty-input error paths, and decoder panic preconditions are checked first.
- The crate remains GUI-free and must not depend on egui or the `image` crate.

## Editing map
- To add a typing hint, place its WebP in `assets/`, add its public constant and
  stable name in `src/typing.rs`, and add it to `typing::ALL`.
- Asset paths must remain publication-allowed by the root `.gitignore`; the existing
  `!crates/ms-gifs/assets/*.webp` rule covers this directory, while a new asset
  directory requires its own allowlist rule.
- To change decoding or RGB normalization, edit `src/lib.rs` and extend the tests.
- To change diagnostic contracts, edit `src/error.rs`.
