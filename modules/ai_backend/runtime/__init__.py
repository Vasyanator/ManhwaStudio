"""
Package: modules/ai_backend/runtime

Process-wide runtime plumbing for the Python AI backend: program-root
resolution, Torch availability probing, device selection, resident-model
leasing and the ROCm-specific runtime workarounds.

This package intentionally exposes NOTHING here - no re-exports and no imports
of its own submodules. Several of them (`device_service`, `rocm_mmap_transfer`)
touch the AI stack, so a re-export would make `import
modules.ai_backend.runtime.paths` drag torch in. Keeping this file empty of
imports is what lets the torch-free `ipc/` layer and its tests use the cheap
members of this package, and it is the same reason the parent package
(`modules/ai_backend/__init__.py`) resolves `run_server` lazily via PEP 562.

Import submodules directly, e.g. `from .runtime.paths import program_root`.
"""
