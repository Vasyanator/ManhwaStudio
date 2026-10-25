/*
File: crates/ms-atomic-file/src/lib.rs

Purpose:
Crate root of `ms-atomic-file`, the one owner of "replace a file's contents atomically" (temp +
fsync + rename) and of the temp-name pattern that recipe produces.

Exports:
- `write_atomic` / `write_atomic_with` + `AtomicWriteOptions` / `Durability`: the replace
  recipe, with the per-site options (parent creation, unix mode, identical-bytes skip,
  unique-per-call temp name).
- `write_temp_file` / `rename_with_retry` / `sync_directory`: the recipe's steps, for
  multi-step commits that validate a staged temp before renaming it (`ms-docstore` whole-document
  writes) and for publishes of already-staged files.
- `temp_path_for` / `is_temp_artifact`: the temp-name pattern and its matcher.
- `AtomicWriteError`: the typed failure (`kind()`, converts into `std::io::Error`).
- `WriteStep` / `FaultPoint` / `recorded_steps` / `arm_fault`: test observability (the last two
  only under `cfg(test)` or the `test-support` feature).

Notes:
Level 0 crate: std + `thiserror` only, no logging (callers log with their own context). It
compiles on every target without `cfg` in its callers: on wasm32 std's file system answers every
call with `io::ErrorKind::Unsupported`, which surfaces as `AtomicWriteError::TempWrite`.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate is named after the concept its types export (`AtomicWriteError`, ...).
#![allow(clippy::module_name_repetitions)]

mod error;
mod naming;
mod recipe;
mod test_hooks;
#[cfg(test)]
mod tests;

pub use error::AtomicWriteError;
pub use naming::{is_temp_artifact, temp_path_for};
pub use recipe::{AtomicWriteOptions, Durability, rename_with_retry, sync_directory, write_atomic, write_atomic_with, write_temp_file};
pub use test_hooks::{FaultPoint, WriteStep};
#[cfg(any(test, feature = "test-support"))]
pub use test_hooks::{arm_fault, recorded_steps};
