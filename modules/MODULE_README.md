# Module: modules

## Purpose

Python support layer for the Rust application. Runtime Python code here backs launcher workflows, browser automation, downloaders, and AI/backend integrations that are not implemented directly in Rust.

## Architecture

Rust owns the main GUI and invokes Python modules only through explicit process or module boundaries. Long-running work must stay outside the GUI thread and report progress or errors through the Rust-facing bridge that started it.

Python modules may use local browser profiles, Selenium, image libraries, and AI/runtime packages, but they must not silently fake missing dependencies or return placeholder outputs. Unsupported inputs should fail with clear user-facing errors and logged technical context.

## Files and submodules

- `new_project/`: helper modules for the launcher "New project" workflows, including browser-driven advanced download.
- `downloader.py`: site-specific quick download helpers used by legacy and launcher flows.
- `browser_f.py`: Selenium browser/profile construction and cookie/header transfer helpers.
- `ai_backend/`: Python AI service runtime called by the Rust application.

## Contracts and invariants

- Browser automation is stateful and tied to the Selenium driver that owns the active page.
- Advanced browser downloads must resolve image links through the current tab session; this includes normal HTTP(S), data URLs, and browser-scoped URLs such as `blob:`.
- Downloaded image bytes must be decoded and validated as real images before returning to Rust.
- Do not add hidden network, model, or package dependencies without explicit errors when they are missing.

## Editing map

- To change advanced browser fetching, see `new_project/adv_fetch_cli.py`.
- To change supported direct downloader sites, see `downloader.py`.
- To change browser construction, profiles, headers, or Selenium cookies, see `browser_f.py`.
