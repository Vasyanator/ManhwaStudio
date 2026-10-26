/*
File: crates/ms-download/src/test_support.rs

Purpose:
Test-only scripted HTTP server for code built on `resumable`: a single-body `RangeFetcher`
that honours ranges and replays a script of drops, short bodies, error statuses and size
announcements. Compiled for this crate's tests and, through the `test-support` feature, for
dependents' tests (enabled only from `[dev-dependencies]`).

Key items:
- `Step`: one scripted answer.
- `ScriptedFetcher`: the server; `offsets()` / `urls()` list every requested offset / URL,
  `with_identity()` makes it announce a `RemoteIdentity`.

Notes:
- No network, no files: everything is in memory.
*/

use std::collections::VecDeque;
use std::io::{Cursor, Read};
use std::sync::Mutex;

use crate::resumable::{FetchError, FetchResponse, FetchStatus, RangeFetcher, RemoteIdentity, chunk_len, classify_status};

/// One scripted answer; once the script is empty the server answers normally.
#[derive(Debug, Clone, Copy)]
pub enum Step {
    /// Serve `n` bytes from the requested offset, then fail the body read (connection reset).
    Drop(usize),
    /// Serve `n` bytes from the requested offset, then end the body cleanly.
    Short(usize),
    /// Answer with an HTTP error status and an optional `Retry-After`.
    Status(u16, Option<&'static str>),
    /// Serve the requested range normally but announce this whole-file size.
    Announce(Option<u64>),
}

/// Single-body server: honours ranges (416 with the real total at or past the end) and
/// replays a script of failures.
#[derive(Debug)]
pub struct ScriptedFetcher {
    body: Vec<u8>,
    script: Mutex<VecDeque<Step>>,
    offsets: Mutex<Vec<u64>>,
    urls: Mutex<Vec<String>>,
    /// Identity every successful response carries (`None`: the server sends no entity tag).
    identity: Option<RemoteIdentity>,
}

impl ScriptedFetcher {
    /// A server for `body` that first replays `script`, one step per request.
    #[must_use]
    pub fn new(body: &[u8], script: &[Step]) -> Self {
        Self { body: body.to_vec(), script: Mutex::new(script.iter().copied().collect()), offsets: Mutex::new(Vec::new()), urls: Mutex::new(Vec::new()), identity: None }
    }

    /// The same server, announcing `identity` on every successful response.
    #[must_use]
    pub fn with_identity(mut self, identity: Option<RemoteIdentity>) -> Self {
        self.identity = identity;
        self
    }

    /// Every offset requested so far, in order (empty if the record lock is poisoned).
    #[must_use]
    pub fn offsets(&self) -> Vec<u64> {
        self.offsets.lock().map(|offsets| offsets.clone()).unwrap_or_default()
    }

    /// Every URL requested so far, in order (empty if the record lock is poisoned).
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.urls.lock().map(|urls| urls.clone()).unwrap_or_default()
    }
}

/// A body reader whose every read fails like a reset connection.
struct ResetReader;

impl Read for ResetReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer"))
    }
}

/// A fake-server internal failure as a permanent fetch error.
fn permanent(err: impl std::fmt::Display) -> FetchError {
    FetchError::Permanent { detail: err.to_string(), status: None }
}

impl RangeFetcher for ScriptedFetcher {
    fn fetch(&self, url: &str, offset: u64) -> Result<FetchResponse, FetchError> {
        self.offsets.lock().map_err(permanent)?.push(offset);
        self.urls.lock().map_err(permanent)?.push(url.to_owned());
        let step = self.script.lock().map_err(permanent)?.pop_front();
        let total = chunk_len(self.body.len());
        if offset > 0 && offset >= total {
            return Err(FetchError::RangeNotSatisfiable { total: Some(total) });
        }
        let start = usize::try_from(offset).map_err(permanent)?;
        let rest = self.body.get(start..).ok_or_else(|| permanent("offset past the body"))?;
        let status = if offset > 0 { FetchStatus::Partial } else { FetchStatus::Full };
        let (total_len, body): (Option<u64>, Box<dyn Read + Send>) = match step {
            None => (Some(total), Box::new(Cursor::new(rest.to_vec()))),
            Some(Step::Drop(len)) => (Some(total), Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec()).chain(ResetReader))),
            Some(Step::Short(len)) => (Some(total), Box::new(Cursor::new(rest[..len.min(rest.len())].to_vec()))),
            Some(Step::Status(code, retry_after)) => return Err(classify_status(code, retry_after)),
            Some(Step::Announce(announced)) => (announced, Box::new(Cursor::new(rest.to_vec()))),
        };
        Ok(FetchResponse { status, total_len, identity: self.identity.clone(), body })
    }
}
