# Module: crates/ms-sdcpp/src/ffi

## Purpose
Hand-written bindings to the stable-diffusion.cpp C API of the pinned release
(`catalog::PINNED`). Crate-private; only the sd.cpp worker process loads the library.

## Architecture
- `types.rs`: `#[repr(C)]` mirrors of the 16 structs the worker touches (and every struct
  nested in them), C enums as `i32` newtypes, callback types, the opaque context.
- `api.rs`: `SdApi::load(dir, row)` — load ggml's dispatcher, register the backends in the
  package directory (`ggml_backend_load_all_from_path`), load sd.cpp, resolve every symbol,
  check `sd_commit()` against `PINNED.short_commit`; safe wrappers of the info calls; the pure
  `classify_load_error` and `parse_device_list`.
- `layout_tests.rs`: size / alignment / every field offset of every mirror, the enum constants
  used, and the sha256 of `vendor/stable-diffusion.h`.

## Contracts and invariants
- The mirrors match `vendor/stable-diffusion.h` byte for byte; its sha256 is
  `PINNED.header_sha256`. Layout constants come from compiling C against `vendor/`
  (`gcc -m64` on Linux; identical under `x86_64-w64-mingw32-gcc`, checked with
  `_Static_assert`s). Never edit a constant to pass a test — re-measure.
- Structs are initialized only by the library's `*_init` functions; Rust writes just the fields
  it needs. Library-allocated results are freed only by the library's free functions.
- `*_to_str` functions are never called. Every `unsafe` block states its `// SAFETY:`.
- `SdApi` keeps the libraries alive as long as the copied function pointers (field order).

## Editing map
- New release: re-vendor the header, regenerate the mirrors, re-measure the layout table.
- New entry point: add the pointer to `SdApi` and its `symbol` line in `load`, with the C
  declaration's exact type.
