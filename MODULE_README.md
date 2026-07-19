# Module: repository root

## Purpose
The repository root contains project-level manifests, packaging scripts, the Python AI backend entry point, and shared configuration used by packaging and the AI backend.

## Architecture
Rust application code lives under `src/` and is the current implementation; it owns install/update/launch via the `manhwastudio_rs` binary. The remaining root Python files are the AI backend entry point (`ai_backend.py`), its shared config (`config.py`), and packaging/dev utilities (`build_zip.py`, `win_release.py`, `*.no_hg.py`). The legacy 2.x Python launcher/install/update entry points (`launcher.py`, `qt_runner.py`, `one_click.py`, `update.py`) and their `Запустить.*`/`Установить.*`/`Обновить.*` shell scripts were superseded by Rust and archived under `old_or_test/2.X/`. Packaging must use explicit include/exclude lists and must not infer release metadata from file names when a typed manifest is available.

## Files and submodules
- `Cargo.toml`: Rust package manifest and authoritative application version.
- `config.py`: shared Python-side runtime constants and JSON-backed user configuration helpers.
- `build_zip.py`: builds `ManhwaStudio.zip`; synchronizes `config.py VERSION` from `Cargo.toml` before archiving.
- `build-all.py`: release orchestrator; builds the desktop target matrix, signs Windows exes, invokes `build_zip.py`, and assembles `target/final/` under updater-expected asset names.
- `modules/`: Python helper modules used by launcher/backend flows.
- `src/`: Rust application source.

## Contracts and invariants
- `Cargo.toml [package].version` is the source of truth for release versioning.
- `config.py VERSION` must be updated before packaging so Python-side update/install code sees the same version as the Rust binary.
- Packaging scripts should fail clearly when required manifests or assignments are missing instead of producing an archive with stale metadata.

## Editing map
- To change the release matrix, signing, or `target/final/` assembly, edit `build-all.py`.
- To change ZIP contents or version synchronization, edit `build_zip.py`.
- To change Python runtime constants, edit `config.py`.
- To change current application behavior, prefer `src/` unless the task explicitly targets legacy Python/runtime packaging.
