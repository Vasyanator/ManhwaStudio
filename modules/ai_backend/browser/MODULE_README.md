# Module: modules/ai_backend/browser

## Purpose
Hosts the advanced web-scraping browser session (Selenium / CloakBrowser) inside the AI backend
process and exposes it through the single IPC method `browser.command`. It is the only
non-AI service domain in the backend: no model, no weights, no device selection.

## Architecture
`server.py` constructs one `BrowserService` and publishes it as `AppState.browser`;
`ipc/handlers/browser.py` reaches it through `HandlerContext.state.browser` and never imports this
package.

The service does not reimplement scraping. It instantiates the daemon class of the selected backend
(`AdvancedFetchDaemon` / `CloakFetchDaemon` under `modules/new_project/`), redirects the daemon's
`_emit` callback into `BrowserService._sink`, and calls the daemon's `_handle_command` directly:

- `progress` events are forwarded to the per-request IPC `ProgressEmitter`, so the launcher gets
  live progress frames;
- the single terminal event (`opened` / `result` / `auto_result` / `link_collect_started` /
  `intercept_count` / …) is captured and returned as the IPC response header;
- an exception out of `_handle_command` propagates, and the dispatcher reports `status:"error"`.

## Files and submodules
- `service.py`: `BrowserService` (`dispatch` / `close` / `health`), backend selection
  (`BACKEND_SELENIUM` / `BACKEND_CLOAK`), the owner-thread executor, the daemon-event sink, and the
  cancel-file bridge. Edit here for anything browser-session related.
- `test_browser_service.py`: covers the thread-affinity contract below with a fake daemon that
  records `threading.get_ident()` per call — no real browser, no Selenium/Playwright needed.

## Contracts and invariants
- **Thread affinity (the reason this service is not a plain adapter).** Playwright's sync objects
  are greenlet-bound to the thread that created the browser context, while the IPC dispatcher hands
  each request to an arbitrary pool worker. All daemon work that touches the browser
  (`_handle_command` and `close`) is therefore marshalled onto ONE dedicated owner thread
  (`_browser_executor`, `max_workers=1`, alive for the service's lifetime), and the daemon is given
  the `_run_on_browser_thread` hook so its own background loop pins its Playwright calls to the same
  thread. Dropping either half reintroduces "Cannot switch to a different thread" at runtime, not at
  import time. `test_browser_service.py` is what keeps this honest.
- A single `RLock` serialises browser commands (mirroring the old single stdin loop). The lock gives
  mutual exclusion; the executor gives the single-thread guarantee — they are not interchangeable.
- Downloaded images are handed to the launcher as an on-disk directory path + count in the response
  header. No image bytes travel over IPC for downloads, and there is no response blob.
- Selenium / Playwright are imported lazily, only when a browser command first runs, so an AI-only
  backend never pays that import cost and a missing scraping dependency cannot break AI startup.
- Cancellation is partial by design: only commands whose long work polls a `cancel_file`
  (`_CANCELABLE_COMMANDS`) honour the IPC `cancel_event`, which is bridged to a temp cancel file.
  Every other command runs to completion, exactly as the stdio daemons did.
- `_NON_RESULT_EVENTS` (`progress` / `log` / `ready`) are never captured as the terminal result;
  a daemon background thread emitting progress outside a dispatch is dropped harmlessly
  (`_active_emitter` is `None` there).
- `__init__.py` re-exports `BrowserService`, `BACKEND_SELENIUM` and `BACKEND_CLOAK`. This is the one
  sub-package of `modules/ai_backend` that re-exports, and it is only safe because `service.py`
  imports no heavy dependency at module level. Do not add an eager Selenium/Playwright import to
  `service.py` without removing the re-export first.

## Editing map
- To add or change a browser command: the command contract belongs to the daemons under
  `modules/new_project/`; only add a case here if it needs progress, cancellation, or capture
  handling that `dispatch` does not already provide.
- To change which daemon backend is used or how it is built: `_normalize_backend` / `_build_daemon`.
- To make another command cancellable: add it to `_CANCELABLE_COMMANDS` and make sure the daemon
  side actually polls its `cancel_file`.
- To change the request/response wire shape: `../ipc/handlers/browser.py` and `../ipc/PROTOCOL.md`.
