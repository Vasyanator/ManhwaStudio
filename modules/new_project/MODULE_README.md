# Module: modules/new_project

## Purpose

Python helpers for the Rust launcher "New project" flows. The active production path is Rust UI code that launches these helpers for browser automation and legacy-compatible import/download operations.

## Architecture

`adv_fetch_cli.py` runs as a line-oriented JSON daemon launched by `src/launcher/new_project/advanced_download.rs`. It owns the Selenium driver, selects the active page, collects image candidates from DOM and shadow DOM, downloads validated image bytes into a temporary folder, and reports progress/results back to Rust. Every daemon event includes `downloader_version` from root `config.VERSION`, and Rust warns when it differs from the Studio binary version.

Browser-owned state stays inside the daemon. Advanced downloader image candidates are fetched through Selenium-executed JavaScript in the active browser tab so cookies, credentials, CSP/runtime URL state, and `blob:` URLs match the page session.

## Files and submodules

- `adv_fetch_cli.py`: Selenium daemon for open-url, candidate fetch, link collection, canvas fetch, and canvas intercept commands.
- `common.py`: shared helpers for URL/pattern handling used by new-project download flows.
- `downloaders.py`: legacy PyQt-facing wrappers for downloader actions.
- `batch_nodes_window/`: Python legacy batch-node UI/runtime retained for compatibility.
- `window.py`: legacy Python new-project window.

## Contracts and invariants

- The daemon protocol is one JSON command per stdin line and one JSON event per stdout line.
- The daemon protocol includes `downloader_version` on stdout events for Rust-side version
  compatibility warnings.
- Selenium driver calls must remain in the daemon-owned browser context; advanced image downloads are intentionally serialized through the active tab session.
- Progress events should remain monotonic for a fetch run.
- Temporary output folders contain only successfully decoded images, named in page order.
- Errors returned to Rust must distinguish user-facing messages from technical log messages.

## Editing map

- To change advanced browser image extraction, edit `adv_fetch_cli.py`.
- To change wildcard/prefix matching shared by download flows, edit `common.py`.
- To change legacy Python UI glue, edit `downloaders.py` or `window.py`.
