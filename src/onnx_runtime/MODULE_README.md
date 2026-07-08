# Module: src/onnx_runtime

## Purpose
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform and execution provider, so the `ms-onnx` crate can
`dlopen` it. It probes a portable cache, and on first use downloads the pinned
official release archive, verifies its SHA256, and extracts the loadable
library (plus any extra sibling DLLs). It delivers dylib RESOLUTION only: it does
not link, initialize, or run onnxruntime. `native_runtime` consumes the returned
path for the CPU / DirectML / CoreML providers.

## Architecture
`config::data_dir()/onnxruntime/<provider_id>/<ort_version>/` is the per-provider,
per-version cache root. Resolution flow (`resolve_or_download_ort_dylib`):

```
probe cache -> (hit) return path, no network
            -> (miss) manifest lookup (os, arch, provider)
                    -> download archive to <name>.part -> atomic rename
                    -> SHA256 verify (mismatch = hard error, delete file)
                    -> extract only the needed library member(s)
                    -> return primary dylib path
```

The manifest (`manifest.rs` + `ort_manifest.json`) is embedded via `include_str!`
and parsed into typed structs. It is pinned to a single ONNX Runtime version
(`ORT_VERSION = 1.20.1`). Only the explicitly listed archive members are unpacked,
so large sidecar files (e.g. the Windows `.pdb`) and symlinks are never touched.

Provider selection reuses `ms_onnx::ExecutionProvider::id()` as the cache-scoping
key. This module produces the dylib path that `ms-onnx` consumes; it never depends
on `ms-onnx`'s loading internals.

## Files and submodules
- `mod.rs`: public API — `OrtRuntimeError`, `OrtDownloadProgress`/`OrtDownloadStage`,
  `ort_dylib_dir`, `resolve_or_download_ort_dylib`. Owns download (ureq streaming),
  archive extraction dispatch (zip / tar.gz / tar.zst), and the cfg-selected
  expected library filename. Edit here for download/extract/progress behavior.
- `manifest.rs`: typed manifest model (`ArchiveKind`, `ManifestEntry`), embedded
  JSON parsing, `lookup`, `current_platform`, and SHA256 helpers
  (`sha256_hex`, `sha256_hex_of_file`, `verify_sha256_file`). Edit here to change
  the manifest schema or integrity checks.
- `ort_manifest.json`: the pinned per-(os, arch, provider) download recipes with
  real SHA256 digests. Edit here to bump the ONNX Runtime version or add providers.

## Contracts and invariants
- Worker-thread only: `resolve_or_download_ort_dylib` performs blocking network and
  disk I/O and MUST NOT run on the GUI thread.
- App-starts-without-library: resolution is lazy and first-use only; a missing
  library is never an error until a caller asks for it.
- Integrity: a pinned SHA256 that mismatches is a HARD error — the archive is
  deleted and resolution aborts; extraction never runs on an unverified-mismatch
  file. When an entry has `sha256: null`, integrity is NOT verified: the actual
  digest is computed and logged via `runtime_log` (a documented, visible gap, not
  a silent fallback).
  - Removal condition for the null-hash gap: paste the logged SHA256 into the
    corresponding `ort_manifest.json` entry. Every non-GPU shipping entry (CPU Linux
    x64 / Windows x64 / macOS x86_64/arm64, DirectML Windows x64, CoreML macOS
    x86_64/arm64) carries a real, verified hash. The two CUDA entries (Windows x64
    zip / Linux x64 tgz) ship with `sha256: null` on purpose: the GPU archives are
    hundreds of MB (Windows ~338 MB, Linux ~258 MB) and were not hashed here, so
    integrity is computed-and-logged, not verified. Removal condition: download each
    GPU archive once, copy the logged SHA256 into its `ort_manifest.json` entry.
- Multi-DLL providers: `extra_members` lists additional archive-internal libraries
  extracted next to the primary `dylib_member` in the SAME cache directory. DirectML
  needs `DirectML.dll` (+ `onnxruntime_providers_shared.dll`) beside `onnxruntime.dll`
  because the DML-enabled `onnxruntime.dll` `dlopen`s `DirectML.dll` by name. The
  DirectML archive is the PyPI `onnxruntime-directml` wheel (a zip); members live
  under `onnxruntime/capi/`. CoreML reuses the standard macOS archive (EP built in).
  CUDA uses the official onnxruntime GPU build (Windows zip / Linux tgz); it extracts
  `onnxruntime_providers_cuda` + `onnxruntime_providers_shared` (the `.dll`/`.so`
  the CUDA-enabled `onnxruntime` library dlopens) next to the primary library. The
  TensorRT provider lib is intentionally omitted (unused; needs extra TensorRT
  system libs). The GPU build depends on a SYSTEM CUDA 12.x + cuDNN 9.x install; the
  app downloads only the onnxruntime GPU dylibs and never bundles CUDA/cuDNN.
- Version floor: onnxruntime must be `>= 1.18` because the pinned `ort`
  (`2.0.0-rc.12`, `api-18`) hard-errors on older libraries. `ORT_VERSION` is 1.20.1.
- No silent provider substitution: a provider with no manifest entry returns
  `OrtRuntimeError::NoManifestEntry`, never a CPU fallback or a panic.
- Path safety: extracted members are flattened to a controlled filename inside the
  cache directory; archive-path traversal and symlink following are impossible.
- Storage location belongs to `config::data_dir()`; do not hard-code writable paths.

## Editing map
- Bump the onnxruntime version or add a platform/provider: `ort_manifest.json`
  (URL, `sha256`, `archive`, `dylib_member`, `extra_members`) and `ORT_VERSION`.
- Change download/progress/extraction behavior: `mod.rs`.
- Change manifest schema, lookup, platform keys, or hashing: `manifest.rs`.
- Consume the resolved dylib path (Phase 1): call `resolve_or_download_ort_dylib`
  from a worker, pass the returned path to `ms_onnx::OrtRuntime::load`.
