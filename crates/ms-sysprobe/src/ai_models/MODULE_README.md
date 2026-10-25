# Module: crates/ms-sysprobe/src/ai_models

## Purpose
Submodules of `ai_models.rs`. Mostly EXTERNAL models: third-party models the application
downloads itself, pinned by commit (Hugging Face) or by exact size + sha256 (plain HTTPS), as
opposed to the unpinned app-managed `Vasyanator2/ManhwaStudio_AI_Models` tree that
`ai_models.rs` resolves. Today: Baberu OCR, the PaddleOCR-VL variants, EasyOCR (14 scripts,
GitHub release zips), Surya detection + recognition (the Surya model host), the SDXL
diffusers configs and the curated SDXL inpainting model. The Python backend never downloads
these; it receives the resolved directory or file paths from Rust and loads offline. Plus
`hf_url.rs`, the URLs BOTH downloaders use; the HTTP stream they share is the `ms-download`
crate.

## Architecture
- `external_catalog.rs` is DATA: one `ExternalModelSpec` per model (`ExternalSource`:
  `HuggingFace { repo_id, revision }` or `Url { base_url }`; file allowlist with exact size +
  sha256; target directory relative to `side_models`; `DirLayout`), the typed
  `PaddleVlVariant` selector, the Baberu layout helper `baberu_files` and `all_specs()`.
- `easyocr.rs` is the ONE owner of EasyOCR decisions: `normalize_easyocr_langs` (raw setting
  -> canonical codes sent to the backend), `easyocr_script_for` (mirror of easyocr 1.7.2
  `Reader.__init__`), `EasyOcrScript`, `easyocr_spec`. Every script spec lives in the ONE
  `Shared` directory `EasyOCR/` (detector `craft_mlt_25k.pth` once + every recognizer, as
  easyocr reads both from one `model_storage_directory`); files are `ZipMember`s of the
  release zips. Its tests compare the tables with `crates/ms-sysprobe/fixtures/easyocr_scripts.json`
  (generated from the installed library, choice cases verified against the real `Reader`).
- `external.rs` is the PINNED downloader: the stat-only probe (`external_model_status`,
  `installed_dir`) and the blocking downloader (`download_external_model`), whose core
  (`download_with`) takes an injected `RangeFetcher` and `RetryPolicy` (with an injected
  backoff sleep), so tests run without network and without real waiting. It owns the
  staging layout, markers and the zip-member flow; one file is fetched and published by
  `ms_download::fetch_verified`, a member extracted by `ms_download::unpack::zip_member_atomic`,
  the per-spec busy guard is `ms_download::busy::BusyGuard`.
- `hf_url.rs` (native-only): `hf_resolve_url` (endpoint from `HF_ENDPOINT`, default
  `https://huggingface.co`, every path segment percent-encoded; no `hf_hub` and no home
  directory, which Android app processes lack) and `url_source_file_url` (same encoding under
  a `Url` source's base).
- Neither reads config: callers pass the side-models root (`ms_config::side_models_dir()`).
- Threading: none of it may run on the GUI thread; the OCR download controller in
  `ms-tab-translation` owns the worker and polls the progress it is given.

Download flow for one spec (`<side_models>/<spec.dir>`; marker and staging per `DirLayout`:
`Exclusive` = `.ms_model_complete.json` + `.download/`, `Shared` =
`.ms_model_complete.<id>.json` + `.download/<id>/`):
1. validate the spec; take the process-wide per-spec busy guard (`Busy` for a second call);
2. move the completion marker into `<staging>/previous_marker.json` (no reader sees
   "installed" while files change; the old file list survives a cancel);
3. discard staged parts that belong to a different spec identity (its file paths are merged
   into `abandoned_files.json` first, because that interrupted download may already have
   published some files), write `in_progress.json`;
4. per `Direct` file: `fetch_verified` with the pin — keep an existing file of exact size
   whose sha256 matches; otherwise resume `<staging>/<path>.part` (prefix rehashed, bound to
   the server's object identity), retrying transient failures; verify size, then sha256;
   fsync; replace-rename into place. Per `ZipMember` file, under the waiting busy id
   `external-member:<final path>` (specs sharing the file serialize): keep a published file
   that hashes to its pin; otherwise `fetch_verified` the archive (size + sha256 pin) into
   `<staging>/<archive>`, `zip_member_atomic` the member onto its path (size + sha256 pin),
   delete the archive;
5. `Exclusive` only: delete files the previous marker or the abandoned list names and this
   spec does not; write the completion marker (`ms_atomic_file`: hidden sibling temp + fsync +
   rename; the staging records use the same writer); remove the staging
   directory (and an empty shared `.download/`; a failure here is only a logged warning: the
   model is installed).

Transient-failure retry (`RetryPolicy::standard`, implemented once in `ms_download::resumable` and
used by both downloaders): connect / DNS / I/O transport errors
(timeouts, resets), any error while reading a body, a body that ends before the pinned size
(ureq reports a short Content-Length body as `UnexpectedEof`, a cut chunked body as a clean
EOF — both land here), and HTTP 408 / 429 / 5xx are retried up to 5 times in a row, waiting
2, 4, 8, 16, 30 s (a delta-seconds `Retry-After` is honoured up to 60 s). Each retry resumes
the `.part` with `Range` from its staged length; the counter resets whenever an attempt
staged new bytes. The wait sleeps in 100 ms slices, polls cancel between them and reports
`ExternalDownloadPhase::Retrying` once per second. Every retry is a `log_warn` with id, file,
offset, attempt, delay and error. Everything else is permanent and ends the download at once.

## Files and submodules
- `external.rs`: spec/source/layout/origin/status/progress/error types, probe, pinned
  downloader, and the repair path `verify_external_model` (hash every file, delete
  mismatching ones plus the markers of every spec listing them). Edit for staging layout,
  marker format, zip-member flow or verification.
- `easyocr.rs` + `easyocr/tests.rs`: EasyOCR language rule, scripts and specs; tests against
  the fixture.
- `hf_url.rs`: resolve / `Url`-source URLs and their tests (encoding, endpoint override).
  Edit for the URL shape or the `HF_ENDPOINT` rule.
- `main_repo_tests.rs`: tests of the main-repository downloader
  `ai_models::ensure_files_with` over `ms_download::test_support::ScriptedFetcher` (publish,
  resume, failure, progress thinning, cancel mid-transfer / mid-backoff / while waiting for
  another download of the same file, identity-bound resume, announced LFS sha256) and of the
  pinned LaMa-MPE path `ensure_pinned_file_with` (repair, keep-by-hash, verified marker).
- `external/tests.rs`: downloader tests over an in-memory fetcher in a self-removing temp
  dir (including hand-built STORED zips for the member flow, shared directories and the
  legacy marker bytes), plus one opt-in `#[ignore]` real download (`MS_TEST_EXTERNAL_DOWNLOAD=1`).
- `external_catalog.rs`: the pinned specs, `PaddleVlVariant`, `all_specs()`. Edit to add a
  model or move a pin.

## Contracts and invariants
- The catalog (with `easyocr.rs` for EasyOCR) is the ONLY owner of source, revision, file
  list and directory of these models; no other crate or the backend spells them.
  `baberu_files` uses the same `const` path strings as the `BABERU_OCR` file table.
- Marker compatibility: a Hugging Face spec of direct files serializes byte-identically to
  markers written before `Url` sources / zip members existed (newer fields are omitted when
  absent), so existing Baberu / PaddleOCR-VL installs keep reading `Installed` (test
  `hugging_face_markers_stay_byte_identical_and_old_installs_read_installed`).
- A `Shared` directory: each spec has its own marker and staging, so install status is per
  spec; a file listed by several specs must carry one identical pin (catalog test); stale
  files are never deleted there (another spec may own them).
- Repair: the status probe is stat-only, so a same-size corruption reads `Installed`;
  `verify_external_model` (worker thread, cancellable) is the only path that rehashes an
  installed model, and it hands repair back to a normal download by deleting the wrong
  files and the affected markers.
- `Url` sources never send the Hugging Face token; `total_bytes`, progress and `Partial`
  count network bytes (`ExternalFile::download_size`: the archive size for a zip member).
- No implicit download: `installed_dir` only reports; `NotDownloaded` is the answer for a
  missing or partial model.
- `Installed` = completion marker equals the spec identity AND every file has its exact
  size (stat-only; hashes are verified at download time). Any pin change therefore reads as
  `Missing` until re-downloaded. `Partial` is reported only while `.download/in_progress.json`
  belongs to this exact spec.
- A published file was always verified (size then sha256) before its rename. A `.part` is
  deleted ONLY on a sha256 mismatch or when the server sends more than the pinned size
  (`SizeMismatch`); a short body, exhausted retries (`Http`) and cancel keep it for resume,
  so the spec then reads `Partial`.
- Durability: a `.part` is fsynced once its stream completes, not periodically. An app crash
  loses nothing; after an OS crash / power loss its unsynced tail may be garbage on some
  filesystems, which the final sha256 check catches (that one file is downloaded again).
  A periodic fsync would not help without a durable "verified up to" checkpoint.
- The HF token goes only into the bearer header, is never logged, and ureq drops it on any
  redirect. Requests resolve through `hf_url::hf_resolve_url`, so `HF_ENDPOINT` is
  honoured; nothing here may call `hf_hub::ApiBuilder::from_env` (it panics without a home
  directory). The main-repository downloader sends no token (public repository).
- Size rule of the shared stream: a pinned size is authoritative; without one the size is
  the 200's `Content-Length` or the 206's `Content-Range` total, requested with
  `Accept-Encoding: identity` (ureq would otherwise negotiate gzip and drop the length). A
  file whose size the server never announced is refused (`.part` kept), never published
  unverified. Staged bytes are deleted only when proven wrong: overflow, a 206 total that
  differs from the one already learned (remote changed), or a 416 showing the `.part` is
  longer than the remote file (that one restarts from zero at once); a matching 416 total
  means the `.part` is already complete.
- `ExternalModelError`'s `Display` is English diagnostic text for logs; user-facing text is
  chosen by the UI layer from the variant (i18n keys live with the UI).
- wasm: specs, status and `installed_dir` compile; `download_external_model` returns
  `Unsupported`; the fetcher, busy guard and download core are native-only.
- Logging: start, completion, cancel and every failure go to `ms_log::runtime_log` with id,
  repo, revision and file.

## Editing map
- New external model: add a spec (and any layout helper) in `external_catalog.rs` and to
  `all_specs()`; the catalog tests check validation, unique ids/dirs and totals — add its
  total there. Pins come from `tools/model_pins/compute_pins.py`.
- New EasyOCR version: regenerate the fixture from the installed library, re-pin the zips,
  update `easyocr.rs` until its tests pass.
- Moving a pin: change revision and the changed file rows together; installs of the old pin
  then read `Missing` and a re-download keeps unchanged files.
- Pinned-download behaviour (verification, marker, staging): `external.rs` +
  `external/tests.rs`. Resume / retry / HTTP behaviour of both downloaders: the `ms-download` crate
  (`crates/ms-download/src/resumable.rs` + its tests; the pinned retry tests stay in
  `external/tests.rs`, the main-repository ones in `main_repo_tests.rs`).
