# Module: crates/ms-download/src

## Purpose
The native-only HTTP download engine of the application: one file streamed into a `.part`
staging file with `Range` resume, automatic retry of transient failures with capped
exponential backoff, connect/read timeouts and cancel; verified publishing of that file;
small in-memory bodies; atomic extraction of one zip member; all-or-nothing staging of several
zip / tar.gz / tar.zst members; plus a process-wide "one download per id" busy guard. It is the mechanism the project's network-download rule (`PROJECT_RULES.md`,
"Network downloads") is implemented with. Foundation layer: GUI-free, no i18n, no config.

## Architecture
- `resumable.rs`: the HTTP seam `RangeFetcher` (`fetch(url, offset)` -> `FetchResponse`, with
  an optional `RemoteIdentity`, or a classified `FetchError`), the production `UreqFetcher`
  (`new(bearer)` or `with_config(FetcherConfig)`: default 15 s connect, 60 s read, optional
  user agent, bearer token and extra headers; proxy from the environment,
  `Accept-Encoding: identity`; follows up to 5 redirects ITSELF so the identity can come from
  a Hugging Face `X-Linked-Etag` on the resolve hop, else the final `ETag`), the retry
  schedule `RetryPolicy` (injected sleep) and the engine `ResumableFetch::fetch_part`, which
  streams the rest of a file into its `.part`, feeds an optional caller `sha2::Sha256`, emits
  `StreamEvent::{Downloading { staged, total }, Retrying}` (`total` = pinned or announced
  size, `None` until known) and fsyncs the completed `.part`.
- `file.rs`: `fetch_verified(engine, FileRequest { url, part, dest, expected }, progress)`
  — waits for the busy id `dest_key(dest)`, keeps a `dest` that already matches the pin,
  resumes the `.part` (rehashing its prefix when a sha256 is pinned) only while the server
  names the same `RemoteIdentity` (recorded in `<part>.identity`; another identity restarts
  from zero), streams the rest via `fetch_part`, checks the sha256 (the pin, or else an
  announced LFS sha256), replace-renames onto `dest` and fsyncs the directory (unix). `Expected::{Announced, Size, SizeSha256}`; events `FileEvent::{Waiting, Verifying,
  Stream}`.
- `small.rs`: `fetch_small(fetcher, retry, cancel, SmallRequest { url, max_bytes }, tag,
  context)` — a capped in-memory body with the same retry / Range-resume rule; a body
  without an announced size is accepted at a clean end of stream (metadata use).
- `unpack.rs`: `zip_member_atomic(archive, member, dest, expected)` — one member into
  `<dest>.unpack`, size-capped, hashed, fsynced, replace-renamed; a matching `dest` is a
  no-op, a concurrent extraction of the same `dest` is `Busy`. `stage_members(archive,
  ArchiveKind, &[WantedMember])` — every wanted member of a zip / tar.gz / tar.zst written,
  hashed and fsynced to `staging_path(dest)` (`<dest>.tmp`), returned as `StagedMember`s in
  wanted order; all or nothing (any failure removes every temporary), publishes nothing;
  typed `UnpackError` per operation. Its one consumer is `ms-native-package`.
- `error.rs`: `DownloadError` (thiserror, English `Display`) for `file`, `small`,
  `zip_member_atomic` and the busy wait; `From<StreamError>`. (`stage_members` has its own
  `unpack::UnpackError`.)
- `busy.rs`: `BusyGuard::try_acquire(id)` -> RAII guard or `Busy`;
  `BusyGuard::acquire_waiting(key, cancel, retry, on_wait)` polls per `wait_slice` until the
  key is free (cancellable); `dest_key(dest)` names a destination file.
- `test_support.rs` (`cfg(test)` or feature `test-support`): `ScriptedFetcher` + `Step`, a
  single-body in-memory server that replays drops, short bodies, statuses and size
  announcements and records the requested offsets and URLs.
- Consumers own the worker thread, the URL, the `.part` and destination paths, the pins,
  markers and user-facing text. They verify and publish through `fetch_verified` /
  `zip_member_atomic` (e.g. `ms_sysprobe::ai_models::external` with size + sha256 pins and
  its own multi-file markers, `ms_sysprobe::ai_models` with `Expected::Announced`), or
  themselves around `fetch_part`. A pinned multi-member archive package with a completion
  marker is `ms-native-package` (built on `fetch_verified` + `stage_members`).

## Files and submodules
- `lib.rs`: crate root; `#![cfg(not(target_arch = "wasm32"))]` — the crate is empty on wasm.
- `resumable.rs`: HTTP seam, ureq fetcher, retry schedule, per-file stream. Edit for HTTP
  handling, retry / resume rules or timeouts.
- `resumable/tests.rs`: the unpinned (server-announced size) stream over `ScriptedFetcher`
  with a recording sleep.
- `file.rs` + `file/tests.rs`: verified single-file download and the shared publish /
  hash helpers.
- `small.rs` + `small/tests.rs`: capped in-memory fetch.
- `unpack.rs` + `unpack/tests.rs`: atomic zip-member extraction and multi-member staging
  (zip / tar.gz / tar.zst; tar keys normalized: `\` -> `/`, a leading `./` stripped;
  directory and symlink entries skipped).
- `error.rs`: `DownloadError`.
- `busy.rs`: the process-wide busy guard, its cancellable wait and `dest_key`.
- `test_support.rs`: the scripted fetcher shared with dependents' tests.
- `test_fs.rs` (`cfg(test)`): temp dirs, content, sha256 and a recording sleep for this
  crate's own tests.

## Contracts and invariants
- Transient (retried): connect / DNS / I/O transport failures, any error while reading a
  body, a body that ends before the size, HTTP 408 / 429 / 5xx (delta-seconds `Retry-After`
  honoured up to `retry_after_cap`). Everything else is permanent. `RetryPolicy::standard`:
  5 retries in a row without progress, 2, 4, 8, 16, 30 s; the counter resets after an
  attempt that staged new bytes; the wait sleeps in 100 ms slices and polls cancel.
- Size rule: a pinned `PartRequest::expected_size` is authoritative; otherwise the size is
  the 200's `Content-Length` or the 206's `Content-Range` total, and a body whose size was
  never announced is refused (`StreamError::Http`, `.part` kept), never returned unverified.
- Staged bytes are deleted only when proven wrong: overflow past the size, a 206 announcing
  a different total than the one already learned (unpinned), or a 416 showing the `.part`
  longer than the remote file (restart from zero). A short body, exhausted retries, a
  permanent failure and cancel keep the `.part` for resume.
- A 206 must start exactly at the requested offset (`UreqFetcher` checks `Content-Range`);
  a 200 to a range request restarts the file and resets the caller's hasher.
- The bearer token is only a request header: never logged; `UreqFetcher` sends it to the
  first hop and to later hops on the first request's own origin (`bearer_allowed`: equal
  scheme + authority, e.g. Hugging Face's same-host resolve-cache redirect), never to another
  host (CDN), and keeps `Range` and the extra headers on every hop.
- Resume identity: `RemoteIdentity::lfs_sha256` is read ONLY from `X-Linked-Etag` (64 hex);
  a plain `ETag` is a version tag, never a hash. A server that sends no tag at all resumes
  without binding (as before); a `.part` without a recorded identity restarts once the
  server names one. A complete `.part` answered by 416 is published without a new identity
  check (it is a whole older version, never a splice).
- Busy ids share ONE process-wide namespace across consumers; `dest:`-prefixed ids
  (`dest_key`) belong to `fetch_verified` / `zip_member_atomic`, which take them
  themselves, so a caller must never hold a `dest:` id around those calls (the guard is not
  reentrant). Callers that share a destination spell its path from the same root.
- Shared destinations (one file used by several catalog models): `fetch_verified` waits
  for the other holder, then finds the file verified and returns without a request;
  `zip_member_atomic` returns `Busy` instead of waiting (no cancel flag), so callers that may
  race on one member serialize fetch + unpack under a key of their own.
- Publishing is a replace-rename of a fully written, fsynced sibling (`.part` /
  `.unpack`); `dest` is never written in place. `std::fs::rename` replaces an existing
  target on Windows as well; the parent directory is fsynced on unix.
- `Expected::Announced` cannot verify an existing `dest`, so `fetch_verified` downloads it
  again; callers decide presence before calling. A malformed sha256 pin is
  `InvalidRequest` before any I/O.
- Logging: retries (`log_warn` with tag, context, offset, attempt, delay, error), restarts
  and ignored ranges go to `ms_log::runtime_log`; the caller logs start / finish / failure.
- Errors are typed (`FetchError`, `StreamError`, `Busy`, `DownloadError`) with English
  diagnostic details and no secrets (`FetcherConfig`'s `Debug` redacts the token and header
  values); consumers map them to their own errors and localized text.
- HTTP status of a permanent failure: `FetchError::Permanent`, `StreamError::Http` and
  `DownloadError::Http` carry `status: Option<u16>` — `Some(code)` when the failure IS an HTTP
  answer (set by `classify_status`, an unexpected status, a 3xx without `Location`, a pinned
  416), `None` for transport and protocol failures, exhausted retries (transient answers keep
  no status) and an unannounced size. Consumers branch on it (e.g. 401 / 403 -> an access
  refusal) instead of parsing `detail`.
- Tests never touch the network: a scripted `RangeFetcher` plus an injected sleep.

## Editing map
- Retry schedule, classification of statuses / transport errors, timeouts: `resumable.rs`
  (`RetryPolicy`, `classify_status`, `transport_is_transient`, `UreqFetcher::new`) + tests.
- Resume / size / deletion rules: `ResumableFetch::{attempt, range_not_satisfiable}`.
- New scripted server behaviour for tests: `test_support.rs` (`Step`).
- A new downloader of one file: `fetch_verified` (plus `zip_member_atomic` for an archive
  member); of a small body: `fetch_small`; of a pinned archive package (several members,
  completion marker): `ms-native-package`; only a multi-file transaction with its own
  markers builds on `fetch_part` directly. Keep the URL, pins and UI text in the consumer.
- Verified publish, shared-destination rule: `file.rs` (`fetch_verified`, `publish_replace`).
- Archive members, archive formats, tar-key normalization: `unpack.rs`.
