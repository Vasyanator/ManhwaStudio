/*
File: crates/ms-download/src/lib.rs

Purpose:
Crate root of `ms-download` — the native-only HTTP download engine shared by every Rust
downloader of the application: one file streamed into a `.part` staging file with `Range`
resume, capped exponential retry of transient failures, connect/read timeouts and cancel,
verified publishing, small in-memory bodies, atomic zip-member extraction, all-or-nothing
staging of several zip / tar.gz / tar.zst members, plus a process-wide "one download per id"
busy guard.

Main responsibilities:
- `resumable`: the HTTP seam (`RangeFetcher`, `UreqFetcher` + `FetcherConfig`), the retry
  schedule (`RetryPolicy`) and the per-file stream (`ResumableFetch::fetch_part`).
- `file`: `fetch_verified` — busy guard on the destination, resume with prefix rehash,
  size / sha256 verification, fsync and replace-rename (`Expected`, `FileRequest`,
  `FileEvent`).
- `small`: `fetch_small` — a capped in-memory body under the same retry / resume rule.
- `unpack`: `zip_member_atomic` — one archive member extracted, verified and renamed;
  `stage_members` — several members staged to `<dest>.tmp`, all or nothing (`UnpackError`).
- `busy`: `BusyGuard::{try_acquire, acquire_waiting}`, `dest_key` — at most one holder per
  id in the process.
- `error`: `DownloadError`, the typed failure of `file`, `small`, `unpack` and the busy wait.
- `test_support` (tests and the `test-support` feature only): a scripted in-memory fetcher.

Contract:
- GUI-free, no i18n, no config, no threads of its own: the caller runs it on a worker,
  builds the URL, chooses the `.part` and destination paths and the pins, and maps the typed
  errors to its own (user-facing) errors. Verification and publishing are either done by the
  caller around `fetch_part`, or delegated to `fetch_verified` / `zip_member_atomic`.
- Native-only: the whole crate is compiled out on wasm (no blocking HTTP stack there).
*/

#![cfg(not(target_arch = "wasm32"))]
#![warn(clippy::all)]

pub mod busy;
pub mod error;
pub mod file;
pub mod resumable;
pub mod small;
#[cfg(test)]
mod test_fs;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod unpack;

pub use error::DownloadError;
pub use file::{Expected, FileEvent, FileRequest, fetch_verified};
pub use small::{SmallRequest, fetch_small};
