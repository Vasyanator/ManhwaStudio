/*
File: crates/ms-download/src/small/tests.rs

Purpose:
Unit tests of `fetch_small`: resume from the received length, the size cap (announced and
received), a body without an announced length, permanent statuses, exhausted retries and
cancel during the backoff.

Notes:
- `ScriptedFetcher` serves the body; the backoff sleep only records. No network.
*/

use std::sync::atomic::AtomicBool;

use super::*;
use crate::test_fs::{SleepLog, content, policy};
use crate::test_support::{ScriptedFetcher, Step};

fn run(fetcher: &ScriptedFetcher, sleep: &SleepLog<'_>, cancel: &AtomicBool, max_bytes: u64) -> Result<Vec<u8>, DownloadError> {
    let sleep_fn = |slice| sleep.sleep(slice);
    let retry = policy(&sleep_fn);
    fetch_small(fetcher, &retry, cancel, &SmallRequest { url: "u", max_bytes }, "[test]", "small=test")
}

#[test]
fn resumes_from_the_received_length() {
    let body = content(200_000, 1);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Drop(70_000), Step::Short(30_000)]);
    let sleep = SleepLog::new(None);
    let result = run(&fetcher, &sleep, &AtomicBool::new(false), 1 << 20);
    assert_eq!(result.ok(), Some(body));
    assert_eq!(fetcher.offsets(), vec![0, 70_000, 100_000]);
    assert!(sleep.count() > 0);
}

#[test]
fn announced_size_past_the_cap_fails_before_reading() {
    let body = content(5_000, 2);
    let fetcher = ScriptedFetcher::new(&body, &[]);
    let result = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), 4_000);
    assert!(matches!(result, Err(DownloadError::SizeMismatch { expected: 4_000, actual: 5_000 })), "{result:?}");
}

#[test]
fn unannounced_body_past_the_cap_fails() {
    let body = content(5_000, 3);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Announce(None)]);
    let result = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), 4_000);
    assert!(matches!(result, Err(DownloadError::SizeMismatch { expected: 4_000, .. })), "{result:?}");
}

#[test]
fn unannounced_body_is_accepted_at_a_clean_end() {
    let body = content(3_000, 4);
    let fetcher = ScriptedFetcher::new(&body, &[Step::Announce(None)]);
    let result = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), 4_000);
    assert_eq!(result.ok(), Some(body));
}

#[test]
fn permanent_status_is_not_retried() {
    let fetcher = ScriptedFetcher::new(b"body", &[Step::Status(404, None)]);
    let sleep = SleepLog::new(None);
    let result = run(&fetcher, &sleep, &AtomicBool::new(false), 100);
    assert!(matches!(result, Err(DownloadError::Http { .. })), "{result:?}");
    assert_eq!(sleep.count(), 0);
}

#[test]
fn retries_are_bounded_without_progress() {
    let fetcher = ScriptedFetcher::new(b"body", &[Step::Status(503, None); 4]);
    let result = run(&fetcher, &SleepLog::new(None), &AtomicBool::new(false), 100);
    assert!(matches!(result, Err(DownloadError::Http { .. })), "{result:?}");
    assert_eq!(fetcher.offsets().len(), 4, "the first attempt plus 3 retries");
}

#[test]
fn cancel_during_the_backoff() {
    let cancel = AtomicBool::new(false);
    let fetcher = ScriptedFetcher::new(b"body", &[Step::Status(429, Some("5"))]);
    let sleep = SleepLog::new(Some(&cancel));
    let result = run(&fetcher, &sleep, &cancel, 100);
    assert!(matches!(result, Err(DownloadError::Cancelled)), "{result:?}");
    assert_eq!(sleep.count(), 1);
}
