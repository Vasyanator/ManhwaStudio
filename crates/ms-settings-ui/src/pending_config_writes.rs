/*
File: crates/ms-settings-ui/src/pending_config_writes.rs

Purpose:
Process-wide count of settings writes (`user_config`, and the project's comic type) that were
handed to a background thread and have not
landed yet, so the process can wait for them once its window has closed instead of losing them.
The GUI thread never waits here while a window is open.

Key structures:
- `PendingConfigWrite`: RAII token; counted from creation (on the thread that queues the write)
  until it is dropped (on the worker, after the write returned).

Key functions:
- `PendingConfigWrite::begin`: count one queued write.
- `wait_for_pending_config_writes`: bounded wait until the count is zero.

Notes:
Users: the General pane's `ConfigKeyWriter` (one token per queued job), the launcher's
detached last-selection writer (`ms_launcher::pages::open_page::spawn_persist_last_selection`)
and the studio's comic-type write (`StudioApp::start_comic_type_save`).
The binary calls `wait_for_pending_config_writes` in `startup` right after the main window's
`eframe::run_native` returned: the window is gone, no frame is left to keep responsive, so a short
bounded wait on the main thread is the documented exception to the GUI-thread rule. The token is
taken BEFORE the worker starts, so a write queued in the very last frame is still waited for.
Native-only use of the wait: a wasm main thread may not block (the web entry never calls it).
*/

use ms_log::runtime_log;
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Number of queued, not yet finished background `user_config` writes.
static PENDING: Mutex<usize> = Mutex::new(0);
/// Signalled whenever [`PENDING`] drops to zero.
static IDLE: Condvar = Condvar::new();

/// One background `user_config` write that has been queued and not finished (see the file
/// header). Create it where the write is queued, move it into the worker, drop it after the
/// write returned (successfully or not).
#[derive(Debug)]
#[must_use = "the write counts as pending only while the token lives"]
pub struct PendingConfigWrite {
    // Private field: tokens are made only by `begin`, so every drop matches one increment.
    _counted: (),
}

impl PendingConfigWrite {
    /// Counts one queued write until the returned token is dropped.
    pub fn begin() -> Self {
        // The counter stays consistent even if a holder panicked: it is a plain integer, updated
        // in one step under the lock, so a poisoned lock's value is still correct.
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        *pending = pending.saturating_add(1);
        Self { _counted: () }
    }
}

impl Drop for PendingConfigWrite {
    fn drop(&mut self) {
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        *pending = pending.saturating_sub(1);
        if *pending == 0 {
            IDLE.notify_all();
        }
    }
}

/// Number of background `user_config` writes queued and not finished right now.
#[must_use]
pub fn pending_config_writes() -> usize {
    *PENDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Blocks until every queued background `user_config` write finished, or `timeout` passed.
/// Returns `true` when none is left. A timeout is logged with the number still pending (those
/// writes are lost when the process exits). Call it only where no window is open (see the file
/// header); never on a wasm main thread.
pub fn wait_for_pending_config_writes(timeout: Duration) -> bool {
    let pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    if *pending == 0 {
        return true;
    }
    runtime_log::log_info(format!("[config-writes] waiting for {} queued settings write(s) before exit", *pending));
    let (pending, result) = IDLE.wait_timeout_while(pending, timeout, |count| *count > 0).unwrap_or_else(PoisonError::into_inner);
    if result.timed_out() && *pending > 0 {
        runtime_log::log_warn(format!(
            "[config-writes] {} settings write(s) still running after {} ms at exit; they may be lost. Possible cause: a slow or busy disk",
            *pending,
            timeout.as_millis()
        ));
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{PendingConfigWrite, wait_for_pending_config_writes};
    use std::time::Duration;

    /// A token held by a worker is waited for; the wait returns once it is dropped. (The count is
    /// process-global, so the test asserts only on its own tokens' effect.)
    #[test]
    fn the_wait_ends_when_the_last_token_drops() {
        let token = PendingConfigWrite::begin();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(token);
        });
        assert!(wait_for_pending_config_writes(Duration::from_secs(10)), "the token is released within the bound");
        assert!(worker.join().is_ok(), "the worker ran to completion");
    }

    /// A token that outlives the bound makes the wait report a timeout.
    #[test]
    fn a_token_held_past_the_bound_times_out() {
        let token = PendingConfigWrite::begin();
        assert!(!wait_for_pending_config_writes(Duration::from_millis(20)));
        drop(token);
    }
}
