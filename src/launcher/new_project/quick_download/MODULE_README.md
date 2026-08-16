# Module: src/launcher/new_project/quick_download

## Purpose
Direct ("quick") chapter downloader of the New Project launcher: it takes one chapter or series URL,
resolves the page image URLs for the supported sites, downloads and decodes them off the GUI thread,
and hands ribbon pages back to `window.rs`.

## Architecture
Four layers, dependencies point downwards only:

```text
controller.rs      worker thread, progress events, ordered parallel download
      |
plan.rs            SiteDownloadPlan / QuickDownloadError + the host dispatch chain
      |
sites/*.rs         one file per supported site: URL shapes, endpoints, markers, decoders
      |
http.rs  url_util.rs  html.rs  base64.rs      site-agnostic primitives
```

`window.rs` only sees `mod.rs`. It calls `QuickDownloadController::begin_download(url)` and drains
`QuickDownloadEvent`s in `poll(ctx)`; everything below the controller runs on the worker thread.
A site module never downloads images itself — it returns a `SiteDownloadPlan` (ordered image URLs
plus the optional `Referer` that site's CDN requires) and the controller does the fetching.

## Files and submodules
- `mod.rs`: module map and the launcher-facing re-exports (`QuickDownloadController`,
  `QuickDownloadEvent`) plus `supported_sites_tooltip()`.
- `controller.rs`: `QuickDownloadController`, the UI/worker event types, `spawn_quick_download`,
  `load_quick_download`, and `download_images_ordered`. Edit it for progress, threading, or the
  ribbon handoff.
- `plan.rs`: `SiteDownloadPlan`, `QuickDownloadError`, and `build_site_download_plan` — the single
  host switch of the module.
- `http.rs`: `execute_request` and the fetch entry points on top of it — `fetch_text`/`fetch_bytes`/
  `fetch_json_value` (default headers) and `fetch_text_with_headers`/`fetch_json_with_headers`/
  `post_json_value` for sites needing custom headers, a fixed cookie, or a JSON POST. Owns the
  shared timeout and User-Agent, `DOWNLOAD_PARALLELISM`, and the wasm stubs.
- `url_util.rs`: URL normalization, host/query/path extraction, relative link resolution,
  `dedupe_preserve`, `looks_like_image_url`. Unit-tested.
- `html.rs`: tolerant tag scanner (`HtmlTag`, `extract_html_tags`), `get_html_attr`,
  `html_unescape`, the anchor/URL collectors, and the JS-literal scanners
  `find_js_array_literal`/`find_js_string_literal` used by sites that embed their page list in an
  inline script. Unit-tested.
- `base64.rs`: standard-alphabet `base64_decode`. Unit-tested.
- `sites/`: per-site chapter resolvers. See `sites/MODULE_README.md`.

## Contracts and invariants
- **Placement rule (the reason this package exists): nothing outside `sites/` may know a host name.**
  URL patterns, API endpoints, HTML markers, per-site decoders and per-site ordering live in exactly
  one file under `sites/`. The host switch in `build_site_download_plan` is the single sanctioned
  exception — it is the dispatch table, not site logic. A helper that names a host belongs in
  `sites/`; a helper that is a generic primitive (base64, percent-decoding, tag scanning) belongs in
  the shared layer even when only one site currently calls it.
- **Adding a site = one file in `sites/` + one arm in `build_site_download_plan`**, plus the host in
  the `supported_sites_hint` locale string (see below). No other file changes.
- **The user-visible list of supported sites is a locale string, not code.**
  `launcher.new_project.quick_dl.supported_sites_hint` is parsed by `supported_quick_download_sites()`
  in `../window.rs` as "header line + one host per line"; keep that shape. A new site must be
  appended there in all five catalogs. ⚠️ `src/locale_store.rs` only ADDS missing keys to an existing
  on-disk `locale/<tag>.json` and never overwrites a value already present, so a machine that has
  already run the app keeps the OLD hint until that file is deleted or patched — a new key reaches
  users, a changed string does not.
- **A new site should not need a new i18n key.** The shared failure keys (`invalid_url_error`,
  `no_chapters_error`, `no_chapter_images_error`, `unexpected_json_error`, `connect_error`,
  `read_response_error`, `site_error_status`) cover the normal cases; adding a key costs five catalog
  edits and is enforced in both directions by `crates/ms-i18n/tests/key_validation.rs` (every `t!`
  key must exist in `en.json`, and every `en.json` key must be referenced from source).
- Nothing here may run on the GUI thread except `QuickDownloadController::poll`, which is
  non-blocking (`try_recv`) by contract.
- Every failure carries both a localized `user_message` and a technical `log_message`
  (`QuickDownloadError`); no silent fallbacks and no placeholder pages.
- Images are decoded from the downloaded bytes, never from the URL extension
  (`looks_like_image_url` is only a link filter).
- `download_images_ordered` is all-or-nothing: the first failure aborts the whole chapter, and the
  returned vector is re-sorted into plan order regardless of completion order.
- The native HTTP client (`ureq`) is not built for wasm. `http.rs` keeps a `#[cfg]` pair for every
  fetch entry point, and the wasm stub returns a clear "unsupported on web" error instead of a fake
  response. Keep both branches in sync.
- The unit tests in `url_util.rs`, `html.rs` and `base64.rs` pin CURRENT behavior including its
  quirks (extension matched anywhere in a URL, `<br/>` parsed with the slash in the tag name,
  `&amp;` unescaped last). Changing a quirk is a behavior change: update the test deliberately, do
  not "fix" it in passing.

## Editing map
- To add or fix a supported site, edit (or add) the file in `sites/` and, for a new site, add one
  arm to `build_site_download_plan` in `plan.rs`.
- To change progress reporting, threading, parallelism use, or the ribbon handoff, edit
  `controller.rs`.
- To change timeouts, headers, or the web-build behavior, edit `http.rs`.
- To change URL/HTML/base64 primitives, edit `url_util.rs` / `html.rs` / `base64.rs` and their tests.
- To change what the launcher can call, edit the re-exports in `mod.rs` (and check
  `../window.rs`).
