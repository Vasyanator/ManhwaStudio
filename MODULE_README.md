# Module: repository root

## Purpose
The repository root contains project-level manifests, packaging scripts, legacy Python runtime entry points, and shared configuration used by the installer and ZIP release flow.

## Architecture
Rust application code lives under `src/` and is the current implementation. Root Python files are packaging/runtime adapters kept for launcher, update, backend, and compatibility flows. Packaging must use explicit include/exclude lists and must not infer release metadata from file names when a typed manifest is available.

## Files and submodules
- `Cargo.toml`: Rust package manifest and authoritative application version.
- `config.py`: shared Python-side runtime constants and JSON-backed user configuration helpers.
- `build_zip.py`: builds `ManhwaStudio.zip`; synchronizes `config.py VERSION` from `Cargo.toml` before archiving.
- `modules/`: Python helper modules used by launcher/backend flows.
- `src/`: Rust application source.

## Contracts and invariants
- `Cargo.toml [package].version` is the source of truth for release versioning.
- `config.py VERSION` must be updated before packaging so Python-side update/install code sees the same version as the Rust binary.
- Packaging scripts should fail clearly when required manifests or assignments are missing instead of producing an archive with stale metadata.

## Editing map
- To change ZIP contents or version synchronization, edit `build_zip.py`.
- To change Python runtime constants, edit `config.py`.
- To change current application behavior, prefer `src/` unless the task explicitly targets legacy Python/runtime packaging.
