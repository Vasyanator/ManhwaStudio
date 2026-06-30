# Module: modules

## Purpose

Python support layer for the Rust application. Runtime Python code here backs the AI backend and
the browser automation it hosts. The legacy 2.x PyQt6 UI (and the non-Qt helpers that only served
it — `model_manager_qt.py`, `psd_import.py`, `utils_qt.py`, `project.py`, `downloader.py`,
`manhwa_merge.py`, `smart_hyphenate.py`, and the PyQt6 parts of `new_project/`) was rewritten in
Rust and moved to `old_or_test/2.X/`; only headless backend/runtime code remains here.

## Architecture

Rust owns the main GUI and invokes Python modules only through explicit process or module boundaries. Long-running work must stay outside the GUI thread and report progress or errors through the Rust-facing bridge that started it.

Python modules may use local browser profiles, Selenium, CloakBrowser/Playwright, image libraries, and AI/runtime packages, but they must not silently fake missing dependencies or return placeholder outputs. Unsupported inputs should fail with clear user-facing errors and logged technical context.

## Files and submodules

- `new_project/`: headless browser-fetch daemons (Selenium / CloakBrowser) hosted in-process by the AI backend; no UI here anymore.
- `browser_f.py`: Selenium browser/profile construction and cookie/header transfer helpers.
- `ai_device.py`: PyTorch/ONNX device selection helpers used by backend services.
- `lama_mpe.py` / `ffc.py`: LaMa MPE inpainting runtime used by the backend.
- `ai_backend/`: Python AI service runtime called by the Rust application.

## Contracts and invariants

- Browser automation is stateful and tied to the selected daemon runtime that owns the active page.
- Advanced browser downloads must resolve image links through the current tab session; this includes normal HTTP(S), data URLs, and browser-scoped URLs such as `blob:`.
- Downloaded image bytes must be decoded and validated as real images before returning to Rust.
- Do not add hidden network, model, or package dependencies without explicit errors when they are missing.

## Editing map

- To change advanced browser fetching, see `new_project/adv_fetch_cli.py`.
- To change browser construction, profiles, headers, or Selenium cookies, see `browser_f.py`.
- To change AI inference services, see `ai_backend/`.
