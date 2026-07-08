# Module: src/onnx_runtime

## Purpose
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform and a selected BUILD, so the `ms-onnx` crate can
`dlopen` it. A **build** is a concrete onnxruntime binary of a specific version
exposing a specific execution-provider set (see `builds.rs`). It probes a portable
cache, and on first use downloads the pinned official release archive, verifies its
SHA256, and extracts the loadable library (plus any extra sibling libs). It delivers
dylib RESOLUTION only: it does not link, initialize, or run onnxruntime.
`native_runtime` consumes the returned path.

## Architecture
`config::data_dir()/onnxruntime/<build_slug>/<version>/` is the per-build,
per-version cache root. Resolution flow (`resolve_or_download_ort_dylib(build)`):

```
manifest lookup_build (os, arch, build) -> (miss) NoManifestEntry, no network
    -> version = entry.version (per-entry, NOT global)
    -> probe cache: ALL expected members present? -> (hit) return path, no network
    -> (miss) for the primary source AND each additional_source:
            download archive to <name>.part -> atomic rename
            -> SHA256 verify (mismatch = hard error, delete file)
            -> extract only the needed library member(s) into the cache dir
    -> return primary dylib path
```

Builds are keyed `(os, arch, provider, build)`. A "build" bundles a provider EP set
with an onnxruntime `version` and binary. Several builds may share one `provider` but
differ by build slug + version — e.g. `cuda12` (1.24.1) and `cuda13` (1.27.0) both
carry `provider = "cuda"`. The cache is scoped by BUILD SLUG (not provider id) so such
builds never collide.

`builds.rs` is the SINGLE SOURCE OF TRUTH the UI and runtime consume: for each build it
declares `slug`, Russian display name, `onnx_version`, `category` (Basic / Specific /
Informational), the ordered EP set, and the platforms that have a manifest entry. It
also owns `default_build_for_current_os` (windows → `directml`, linux → `webgpu`,
macos → `coreml`, else `cpu`) and `default_build_for_provider` (the provider→build map
the compat shims use).

Per-build versions: cpu / coreml / webgpu = **1.27.0**, directml = **1.24.4**,
cuda13 = **1.27.0**, cuda12 = **1.24.1**, openvino = **1.24.1**. `ORT_VERSION = 1.20.1`
is only a legacy fallback floor (no build ships it). The looked-up entry's `version`
scopes both the cache directory and the versioned library filename, so the lookup runs
BEFORE the probe. Only the explicitly listed archive members are unpacked, so large
sidecar files (e.g. the Windows `.pdb`) and symlinks are never touched. Tar keys are
normalized by stripping a leading `./` so the GNU-tar-packed 1.27.0 macOS archives
(which prefix members with `./`) match clean manifest paths.

Multi-source entries: an entry may name `additional_sources` (extra archives) whose
libraries are extracted into the SAME cache directory as the primary source, using
one shared download+verify+extract path (`fetch_source`). The probe fast path skips
the download only when EVERY expected member (primary + extras + additional-source
members) is already on disk, so a partial cache always re-fetches.

This module produces the dylib path that `ms-onnx` consumes; it never depends on
`ms-onnx`'s loading internals.

## Files and submodules
- `mod.rs`: public API — `OrtRuntimeError`, `OrtDownloadProgress`/`OrtDownloadStage`,
  `ort_dylib_dir`, `resolve_or_download_ort_dylib(build, ...)`. Owns download (ureq
  streaming), archive extraction dispatch (zip / tar.gz / tar.zst, with `./` tar-key
  normalization), and the cfg-selected expected library filename. Edit here for
  download/extract/progress behavior.
- `builds.rs`: the static build catalog (`OrtBuild`, `BuildCategory`) and its API
  (`all_builds`, `build_by_slug`, `build_execution_providers`,
  `default_build_for_current_os`, `default_build_for_provider`). Edit here to add a
  build, change a build's EP set/category/default policy, or its display name.
- `manifest.rs`: typed manifest model (`ArchiveKind`, `ManifestSource`,
  `ManifestEntry` with its `build` field), embedded JSON parsing, `lookup`
  (four-key), `lookup_build` (the resolution primary), `build_version`,
  `provider_version` (compat shim), `current_platform`, and SHA256 helpers. Edit here
  to change the manifest schema or integrity checks.
- `ort_manifest.json`: the pinned per-`(os, arch, provider, build)` download recipes,
  each carrying its `build` slug, `version`, and (optionally) `additional_sources`.
  Edit here to bump a build's version, add a platform, or add a build.

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
    corresponding `ort_manifest.json` entry. Every non-GPU shipping build carries a
    real, verified hash: CPU (Linux x64 / Windows x64 / macOS arm64) and CoreML
    (macOS arm64) use GitHub release digests; DirectML, WebGPU, and OpenVINO use PyPI
    wheel digests. Only the four large GPU builds — cuda13 (Windows/Linux, 1.27.0) and
    cuda12 (Windows/Linux, 1.24.1) — ship with `sha256: null` on purpose: the archives
    are hundreds of MB (cuda13 win ~329 MB / linux ~212 MB; cuda12 win ~281 MB / linux
    ~205 MB) and integrity is computed-and-logged, not verified. Removal condition:
    download each GPU archive once, copy the logged SHA256 into its entry.
- Multi-lib builds: `extra_members` lists additional archive-internal libraries
  extracted next to the primary `dylib_member` in the SAME cache directory. DirectML
  (PyPI `onnxruntime-directml` **1.24.4** wheel, members under `onnxruntime/capi/`)
  needs `DirectML.dll` + `onnxruntime_providers_shared.dll` beside `onnxruntime.dll`.
  CoreML reuses the standard macOS **1.27.0** arm64 archive (EP built in). The CUDA
  builds use the official onnxruntime GPU archives and extract the CUDA + TensorRT +
  shared provider libs (`onnxruntime_providers_cuda`, `onnxruntime_providers_tensorrt`,
  `onnxruntime_providers_shared`) next to the primary library; both builds expose the
  `[Cuda, TensorRt, Cpu]` EP set. The GPU builds depend on a SYSTEM CUDA + cuDNN
  install (cuda12 → CUDA 12.x, cuda13 → CUDA 13.x); the app downloads only the
  onnxruntime GPU dylibs and never bundles CUDA/cuDNN.
- OpenVINO: PyPI `onnxruntime-openvino` **1.24.1** wheels. The LINUX wheel bundles the
  full OpenVINO runtime — its `extra_members` include the provider bridge plus the
  OpenVINO core (`libopenvino.so.2541`, `libopenvino_c.so.2541`, the ONNX frontend, the
  intel CPU/GPU/NPU + auto/hetero plugins, and TBB). The WINDOWS wheel does NOT bundle
  the OpenVINO runtime (only the provider bridge DLL): the openvino build needs a SYSTEM
  OpenVINO install on Windows.
- Multi-source entries: `additional_sources` names extra archives whose `members`
  are extracted into the SAME cache directory as the primary source (the primary and
  every additional source share `fetch_source`). No shipped entry uses it today (all
  builds are single-archive); it exists so a build that must bundle libs from a second
  archive can do so without new extraction code.
- WebGPU: the PyPI `onnxruntime-webgpu` **1.27.0** wheel (a zip; native libs under
  `onnxruntime/capi/`) per platform — a full onnxruntime with the WebGPU EP and the
  **Dawn** runtime STATICALLY LINKED into the main library (no separate `webgpu_dawn`
  sidecar in 1.27.0). On Windows Dawn's D3D12 backend needs `dxil.dll` + `dxcompiler.dll`,
  which the 1.27.0 Windows wheel already bundles (so they are `extra_members`). Linux
  (Vulkan) and macOS (Metal) need no extras. WebGPU output is NOT byte-equal to CPU.
- QNN: catalog-INFORMATIONAL only. `builds.rs` lists it (slug `qnn`, category
  `Informational`) so the panel can show it under "Недоступные", but it has NO manifest
  entry and NO execution provider: the QNN EP is a Windows-on-ARM (Snapdragon) backend
  and is not runnable on x86_64.
- Version floor: onnxruntime must be `>= 1.18` because the pinned `ort`
  (`2.0.0-rc.12`, `api-18`) hard-errors on older libraries. Versions are per-build (see
  Architecture). `build_version(build)` reports the version a build resolves at on the
  current platform; `provider_version(provider)` is a compat shim over the provider's
  default build.
- CPU/CoreML re-validation caveat: the CPU and CoreML builds were bumped 1.20.1 → 1.27.0.
  Any parity / end-to-end tests that pinned 1.20.1 CPU numeric output must be re-validated
  against 1.27.0 (numerics may differ within tolerance). onnxruntime 1.27.0 ships NO
  osx-x86_64 archive, so the CPU build drops Intel macOS (Apple Silicon only there).
- No silent build substitution: a build with no manifest entry returns
  `OrtRuntimeError::NoManifestEntry`, never a CPU fallback or a panic.
- Path safety: extracted members are flattened to a controlled filename inside the
  cache directory; archive-path traversal and symlink following are impossible.
- Storage location belongs to `config::data_dir()`; do not hard-code writable paths.

## Editing map
- Bump a build's version, add a platform, or add a build: `ort_manifest.json` (per-entry
  `provider`, `build`, `version`, `url`, `sha256`, `archive`, `dylib_member`,
  `extra_members`, and optional `additional_sources`) AND the `builds.rs` catalog row.
  `ORT_VERSION` is only the legacy 1.20.1 fallback floor.
- Change a build's EP set, category, display name, or the per-OS/per-provider default:
  `builds.rs`.
- Add a build that needs libraries from a second archive: add `additional_sources` to
  its `ort_manifest.json` entry; no code change (the loader already fetches them).
- Change download/progress/extraction behavior: `mod.rs`.
- Change manifest schema, lookup, platform keys, hashing, or the version shims:
  `manifest.rs`.
- Resolve a build's dylib: call `resolve_or_download_ort_dylib(build, ...)` from a
  worker, pass the returned path to `ms_onnx::OrtRuntime::load`.
