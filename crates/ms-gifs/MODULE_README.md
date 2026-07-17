# Module: crates/ms-gifs

## Purpose
GUI-free storage and streaming playback for animated WebP help hints used by
ManhwaStudio. The assets live in this small crate so ordinary edits to the main
crate's `src/` do not recompile roughly 2.9 MB of embedded bytes.

## Architecture
`typing` defines each asset as a `Hint`, combining a stable name with bytes
embedded by `include_bytes!`. `Player::open` validates the WebP animation and
creates an `image-webp` sequential decoder without decoding a frame. Each
`Player::next_frame` call advances compositing by one frame, normalizes RGB output
to RGBA when needed, and transparently resets the decoder at the loop boundary.
The crate has no dependency on egui or the `image` crate; GUI consumers perform
texture upload separately.

Data flow: `typing::<HINT>` -> `Player::open(Hint)` -> repeated
`Player::next_frame(&mut rgba)` -> background-thread GUI consumer.

## Files and submodules
- `src/lib.rs`: public hint inventory access and sequential looping player.
- `src/typing.rs`: stable typing-hint identities and `include_bytes!` declarations.
- `src/error.rs`: typed validation, buffer-contract, and decoder failures.
- `assets/`: source animated WebP files embedded into the library artifact.
- `tests/assets_decode.rs`: memory-light CI validation of shipped asset playback.

## Contracts and invariants
- `Hint::name()` is a unique, stable cache key and must not change across runs.
- Assets are embedded with `include_bytes!`, so this crate recompiles when an asset
  changes while unrelated main-crate source edits do not rebuild the asset bytes.
- `Player::open` validates animation status, non-zero dimensions, decoder layout,
  and a non-zero declared frame count before public frame reads can occur.
- `Player::next_frame` writes non-premultiplied RGBA8 into an exactly sized caller
  buffer and returns the WebP frame's millisecond display duration.
- Playback memory is one `image-webp` RGBA compositing canvas (`w*h*4`), plus an
  RGB scratch buffer (`w*h*3`) only for an alpha-less asset, independent of frame count.
- Per-frame decoding performs CPU work and must never run on the GUI thread.
- Invalid, static, empty, exhausted-after-reset, or wrongly buffered playback returns
  `GifError`; validated decoder panic preconditions remain unreachable.
- The crate remains GUI-free and must not depend on egui or the `image` crate.

## Editing map
- To add a typing hint, place its WebP in `assets/`, add its public constant and
  stable name in `src/typing.rs`, and add it to `typing::ALL`.
- Asset paths must remain publication-allowed by the root `.gitignore`; the existing
  `!crates/ms-gifs/assets/*.webp` rule covers this directory, while a new asset
  directory requires its own allowlist rule.
- To change playback or RGB normalization, edit `src/lib.rs` and extend the tests.
- To change diagnostic contracts, edit `src/error.rs`.
