# Module: crates/ms-sysprobe/src/ai_models

## Purpose
Submodules of `ai_models.rs` for EXTERNAL models: third-party Hugging Face repositories the
application downloads itself at a pinned commit, as opposed to the app-managed
`Vasyanator2/ManhwaStudio_AI_Models` tree that `ai_models.rs` resolves. Today: Baberu OCR and
the PaddleOCR-VL variants (official 1.6 default, official 1.5, manga_ja). The Python backend
never downloads these; it receives the resolved directory or file paths from Rust.

## Architecture
- `external_catalog.rs` is DATA: one `ExternalModelSpec` per model (repo id, 40-hex commit,
  file allowlist with exact size + sha256, target directory relative to `side_models`), the
  typed `PaddleVlVariant` selector and the Baberu layout helper `baberu_files`.
- `external.rs` is MECHANICS: the stat-only probe (`external_model_status`,
  `installed_dir`) and the blocking downloader (`download_external_model`), whose core
  (`download_with`) takes an injected `RangeFetcher` and `RetryPolicy` (with an injected
  backoff sleep), so tests run without network and without real waiting.
- Neither reads config: callers pass the side-models root (`ms_config::side_models_dir()`).
- Threading: none of it may run on the GUI thread; the OCR download controller in
  `ms-tab-translation` owns the worker and polls the progress it is given.

Download flow for one spec (`<side_models>/<spec.dir>`):
1. validate the spec; take the process-wide per-spec busy guard (`Busy` for a second call);
2. move the completion marker into `.download/previous_marker.json` (no reader sees
   "installed" while files change; the old file list survives a cancel);
3. discard staged parts that belong to a different spec identity (its file paths are merged
   into `.download/abandoned_files.json` first, because that interrupted download may
   already have published some files), write `.download/in_progress.json`;
4. per file: keep an existing file of exact size whose sha256 matches; otherwise resume
   `.download/<path>.part` with `Range` (206 appends, 200 restarts, an over-long part is
   deleted), checking cancel and reporting progress after every read (at most 128 KiB, in
   practice one socket read); a transient failure is retried in place (below); verify
   size, then sha256; fsync; rename into place;
5. delete files the previous marker or the abandoned list names and this spec does not;
   write the completion marker `.ms_model_complete.json` (temp + fsync + rename); remove
   `.download/` (a failure here is only a logged warning: the model is installed).

Transient-failure retry (`RetryPolicy::standard`): connect / DNS / I/O transport errors
(timeouts, resets), any error while reading a body, a body that ends before the pinned size
(ureq reports a short Content-Length body as `UnexpectedEof`, a cut chunked body as a clean
EOF — both land here), and HTTP 408 / 429 / 5xx are retried up to 5 times in a row, waiting
2, 4, 8, 16, 30 s (a delta-seconds `Retry-After` is honoured up to 60 s). Each retry resumes
the `.part` with `Range` from its staged length; the counter resets whenever an attempt
staged new bytes. The wait sleeps in 100 ms slices, polls cancel between them and reports
`ExternalDownloadPhase::Retrying` once per second. Every retry is a `log_warn` with id, file,
offset, attempt, delay and error. Everything else is permanent and ends the download at once.

## Files and submodules
- `external.rs`: spec/status/progress/error types, probe, downloader, ureq fetcher, busy
  guard. Edit for download mechanics, marker format or HTTP handling.
- `external/tests.rs`: downloader tests over an in-memory fetcher in a self-removing temp
  dir, plus one opt-in `#[ignore]` real download (`MS_TEST_EXTERNAL_DOWNLOAD=1`).
- `external_catalog.rs`: the pinned specs and `PaddleVlVariant`. Edit to add a model or
  move a pin.

## Contracts and invariants
- The catalog is the ONLY owner of repo id, revision, file list and directory of these
  models; no other crate or the backend spells them. `baberu_files` uses the same `const`
  path strings as the `BABERU_OCR` file table.
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
  redirect. Requests resolve through `hf_hub` URLs, so `HF_ENDPOINT` is honoured.
- `ExternalModelError`'s `Display` is English diagnostic text for logs; user-facing text is
  chosen by the UI layer from the variant (i18n keys live with the UI).
- wasm: specs, status and `installed_dir` compile; `download_external_model` returns
  `Unsupported`; the fetcher, busy guard and download core are native-only.
- Logging: start, completion, cancel and every failure go to `ms_log::runtime_log` with id,
  repo, revision and file.

## Editing map
- New external model: add a spec (and any layout helper) in `external_catalog.rs`; the
  catalog tests check hex lengths, unique dirs and totals — add its total there.
- Moving a pin: change revision and the changed file rows together; installs of the old pin
  then read `Missing` and a re-download keeps unchanged files.
- Download behaviour (resume, verification, marker): `external.rs` + `external/tests.rs`.
