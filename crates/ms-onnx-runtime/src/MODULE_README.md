# Module: crates/ms-onnx-runtime/src

## Purpose
App-layer loader that lazily resolves the official ONNX Runtime dynamic library
for the current platform and a selected BUILD, so the `ms-onnx` crate can
`dlopen` it. A **build** is a concrete onnxruntime binary of a specific version
exposing a specific execution-provider set (see `builds.rs`). It probes a portable
cache, and on first use downloads the pinned official release archive through
`ms-download` (resumable, retried, cancellable, exact size + SHA256 verified), and
extracts the loadable library (plus any extra sibling libs) atomically. On Android the
library is BUNDLED in the APK instead (official `onnxruntime-android` AAR) and is only
located, never downloaded. It delivers
dylib RESOLUTION only: it does not link, initialize, or run onnxruntime.
`native_runtime` consumes the returned path. The binary names the crate directly as
`ms_onnx_runtime::` (native-only).

## Architecture
`ms_config::data_dir()/onnxruntime/<build_slug>/<version>/` is the per-build,
per-version cache root. Resolution flow (`resolve_or_download_ort_dylib(build, cancel,
progress)`; the download-row half is `resolve_download_entry` over an injected fetcher):

```
PlatformFeature::NativeOnnxRuntime unavailable (wasm) -> Unavailable, nothing touched
manifest lookup_build (os, arch, build) -> (miss) NoManifestEntry, no network
    -> (bundled row) <package nativeLibraryDir>/<library>, is_file -> return path
                     (else BundledLibraryMissing / NativeLibraryDir), no network, no cache
    -> (download row) version = entry.version (per-entry, NOT global)
    -> probe cache: marker `.ort-cache-complete` matches this row's recipe AND every
       expected member exists at its recorded size? -> (hit) return path, no network
    -> (miss) BusyGuard::acquire_waiting("ort:<build>:<version>")  [WaitingForOtherDownload]
    -> re-probe (the other holder may have finished) -> (hit) return path
    -> remove the marker + fsync dir (BEFORE any member is replaced)
    -> for each additional_source, then the primary source (published LAST):
            claim .download/<name>.part (its .part.id = url + sha256; foreign -> delete)
            -> ms_download::fetch_verified(SizeSha256): resume, retry [Retrying],
               hash, mismatch deletes the .part, fsync + rename to .download/<name>
            -> extract wanted members to <dest>.tmp (fsync), all found? rename each
               (primary library last), fsync dir; else remove the temporaries
            -> delete the archive
    -> write the marker (recipe + `member <name> <size> <sha256>` lines) via .tmp,
       fsync, rename, fsync dir
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
macos → `coreml`, android → `xnnpack`, else `cpu`) and `default_build_for_provider` (the provider→build map
the compat shims use).

Bundled builds (Android aarch64): `cpu` (`[Cpu]`) and `xnnpack` (`[Xnnpack, Cpu]`, the
default) are two EP sets over ONE bundled `libonnxruntime.so`. They are rows of the separate
`bundled` table of `ort_manifest.json` (`{os, arch, provider, build, version, library}`), so
the download schema keeps `url` / `archive` / `dylib_member` required; `lookup_build` returns
`BuildSource::{Download, Bundled}`, so `build_version`, `build_shipped_here` and the crash-guard
scope key work unchanged for both. The bundled version is pinned twice — the AAR coordinate in
`android/app/build.gradle.kts` and the `bundled` rows — and a host test
(`bundled_version_matches_the_gradle_aar_pin`) keeps them equal. `build_source_kind_here` and
`ort_downloads_offered_here` tell the AI panel which builds have nothing to download.

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
- `lib.rs`: crate root and public API — `OrtRuntimeError`, `OrtDownloadProgress`/`OrtDownloadStage`,
  `ort_dylib_dir`, `resolve_or_download_ort_dylib(build, cancel, progress)`,
  `ort_build_installed(build)` (the single owner of "is this build installed": the same
  marker + member-size rule as the resolver's fast path; stat-only, worker thread). Owns the
  per-build busy wait, the `.part` identity (`claim_part`), the mapping of `ms-download`
  events / errors onto progress / `OrtRuntimeError`, atomic archive extraction (zip /
  tar.gz / tar.zst, with `./` tar-key normalization), and the cfg-selected expected
  library filename. Edit here for download/extract/progress behavior; the HTTP, retry and
  verification mechanics themselves are `ms-download`'s.
- `builds.rs`: the static build catalog (`OrtBuild`, `BuildCategory`) and its API
  (`all_builds`, `build_by_slug`, `build_execution_providers`,
  `default_build_for_current_os`, `default_build_for_provider`). Edit here to add a
  build, change a build's EP set/category/default policy, or its display name. The
  `display_name` field is a plain product name for most builds but a localization
  catalog KEY when the label needs translating (the const array cannot call `t!`);
  read it through `OrtBuild::display_label`, which resolves the key with a fallback
  to the literal.
- `manifest.rs`: typed manifest model (`ArchiveKind`, `ManifestSource`,
  `ManifestEntry` with its `build` field), embedded JSON parsing, `lookup`
  (four-key), `lookup_build` (the resolution primary), `build_version`,
  `build_shipped_here` (allocation-free "has a row here", used by the native availability rule),
  `build_source_kind_here` / `ort_downloads_offered_here` (download vs bundled, for the panel),
  `BundledEntry` / `BuildSource`,
  `provider_version` (compat shim), `current_platform` over the pure `platform_keys`
  (`android` is its own os key with only bundled rows, never `linux`). Edit here
  to change the manifest schema or the pins (`size` + `sha256` per source).
- `ort_manifest.json`: the pinned per-`(os, arch, provider, build)` download recipes,
  each carrying its `build` slug, `version`, and (optionally) `additional_sources`; plus the
  `bundled` table (libraries shipped inside the app package, Android only).
  Edit here to bump a build's version, add a platform, or add a build.

## Contracts and invariants
- Worker-thread only: `resolve_or_download_ort_dylib` performs blocking network and
  disk I/O (and, for a bundled build on Android, a JNI round trip through
  `ms_platform::native_libs`) and MUST NOT run on the GUI thread.
- Bundled rows: Android only (test `bundled_rows_are_android_only_and_match_the_catalog`), a
  `(os, arch, build)` key is in at most one table, and a missing bundled library is the typed
  `BundledLibraryMissing` (logged with the path), never a download.
- App-starts-without-library: resolution is lazy and first-use only; a missing
  library is never an error until a caller asks for it.
- Integrity: every download source pins `size` and `sha256` (required fields: an unpinned
  row does not parse; tests `every_source_pins_size_and_sha256`,
  `a_row_without_sha256_or_size_does_not_parse`). GitHub release assets use GitHub's
  digests, PyPI wheels the PyPI digests. A mismatch is a HARD error
  (`ChecksumMismatch`): the `.part` is deleted and nothing is extracted.
- Network contract (`PROJECT_RULES.md` "Network downloads"): `ms-download`'s standard
  retry schedule and timeouts; the `.part` survives a failure, a cancel or an app exit and
  is resumed by the next call; the sidecar `<archive>.part.id` (url + sha256) discards a
  `.part` staged for another manifest row (missing or unreadable identity = discard).
  Retries surface as `OrtDownloadStage::Retrying`, a cancel as `OrtRuntimeError::Cancelled`.
- One resolution per build at a time in the process (busy id `ort:<build>:<version>`):
  the settings panel and a native OCR load never write one build's files concurrently;
  the second caller waits (`WaitingForOtherDownload`) and then takes the fast path.
- Atomic extraction: a member is published only by renaming its fsynced `<dest>.tmp`, and
  only once every member of the archive was found; the primary library is renamed last.
- Cache trust = the completion marker (`CACHE_MARKER`, format line `CACHE_MARKER_FORMAT`):
  its recipe header (format, build, version, every source's url + sha256 in fetch order)
  must equal the current manifest row, it must list exactly the expected member file
  names, and each must exist at the recorded size (sizes only on the fast path; the
  recorded sha256 is diagnostic). The marker is removed durably before the first member
  rename and written atomically after the last, so a crash mid-publication, a truncated
  member, a re-pinned row or a cache from a program without markers is rebuilt — from the
  still-staged verified archive when it exists (`fetch_verified` keeps a matching `dest`),
  else by downloading again.
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
- Android library name: the AAR's `libonnxruntime.so` has an unversioned SONAME, so
  `expected_primary_dylib_filename` returns `libonnxruntime.so` there (only a future Android
  DOWNLOAD row would use it; bundled rows name their `library`).
- No silent build substitution: a build with no manifest entry returns
  `OrtRuntimeError::NoManifestEntry`, never a CPU fallback or a panic.
- Path safety: extracted members are flattened to a controlled filename inside the
  cache directory; archive-path traversal and symlink following are impossible.
- Storage location belongs to `config::data_dir()`; do not hard-code writable paths.

## Editing map
- Bump the Android ORT: the AAR version in `android/app/build.gradle.kts`, the `bundled`
  rows' `version` and the `cpu` / `xnnpack` catalog `onnx_version` together (tests pin all
  three; a different `cpu` version per OS is not expressible today).
- Bump a build's version, add a platform, or add a build: `ort_manifest.json` (per-entry
  `provider`, `build`, `version`, `url`, `sha256`, `size`, `archive`, `dylib_member`,
  `extra_members`, and optional `additional_sources`) AND the `builds.rs` catalog row.
  `ORT_VERSION` is only the legacy 1.20.1 fallback floor.
- Change a build's EP set, category, display name, or the per-OS/per-provider default:
  `builds.rs`.
- Add a build that needs libraries from a second archive: add `additional_sources` to
  its `ort_manifest.json` entry; no code change (the loader already fetches them).
- Change download/progress/extraction behavior: `lib.rs`.
- Change manifest schema, lookup, platform keys, hashing, or the version shims:
  `manifest.rs`.
- Resolve a build's dylib: call `resolve_or_download_ort_dylib(build, cancel, progress)`
  from a worker, pass the returned path to `ms_onnx::OrtRuntime::load`.
