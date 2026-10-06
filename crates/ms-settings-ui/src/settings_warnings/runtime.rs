/*
FILE OVERVIEW: crates/ms-settings-ui/src/settings_warnings/runtime.rs
The settings-warnings runtime: the GUI-side owner of the `WarningSet` and the ONE
long-lived worker thread that runs the checks.

Purpose:
`SettingsWarnings::start` spawns the named `settings-warnings` worker (`ms_thread`) and
queues a full run of `SettingKey::ALL`. `recheck` stamps the keys a `SettingChange`
affects with a fresh generation and queues them; `poll` (GUI thread, non-blocking)
applies a result only when its generation is the latest requested for that key, so a
slow stale run can never overwrite a newer recheck.

Key items:
- `CheckContext`: GUI-side facts the worker cannot read from config (`--no-ai`, the
  supervisor's autostart value, `--ignore-installed`).
- `SettingsWarnings`: `start`, `disabled`, `recheck`, `poll`, `set`.
- `KeyOutcome` / `CheckRequest` / `CheckBatch`: the channel messages.
- `coalesce` / `worker_loop`: the worker side (native only).

Notes:
The worker blocks on `recv`, drains queued requests to coalesce them (union of keys; per
key the max generation AND the context of the request that carried it), runs the checks
once per distinct context, sends one batch, then requests a repaint. Contexts are never
merged across requests, so a key is always checked with the context it was requested
with. An explicit `SettingChange::BackendAutostart(v)` is latched and overrides the
snapshot-derived autostart of every later request (the snapshot lags the toggle). The
worker exits when the request channel disconnects (the owner was dropped) or when its
result can no longer be delivered. On wasm, or when the spawn fails, the instance is
inert: an empty set, `recheck` / `poll` do nothing.
*/

use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use ms_log::runtime_log;

use super::model::{SettingChange, SettingKey, SettingWarning, WarningSet};
use crate::ai_backend_supervisor::AiBackendHandle;

/// GUI-side facts the checks need but cannot read from config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckContext {
    /// `false` under `--no-ai`: every AI check is then silent.
    pub ai_enabled: bool,
    /// The supervisor's current autostart toggle (it persists the toggle asynchronously,
    /// so config may still hold the old value).
    pub backend_autostart: bool,
    /// `false` under `--ignore-installed`: the System registration tab is read-only then,
    /// so every registration check is silent (a badge must be clearable from its tab).
    pub registration_checks: bool,
}

impl CheckContext {
    /// Captures the context from the app-global backend handle (one short mutex read of
    /// the process snapshot; GUI thread safe) plus `registration_checks` (`false` when the
    /// process runs with `--ignore-installed`).
    #[must_use]
    pub fn from_handle(handle: &AiBackendHandle, registration_checks: bool) -> Self {
        Self {
            ai_enabled: handle.ai_enabled,
            backend_autostart: handle.process_snapshot().auto_start(),
            registration_checks,
        }
    }
}

// The three channel messages below are built and read only by the native worker and
// checks; the wasm build compiles their GUI half alone. `allow` rather than `expect`: the
// wasm target cannot be checked here (`cargo +nightly wcheck` is broken upstream), so an
// `expect` that stopped being fulfilled there could not be noticed and fixed.

/// The result of checking one key.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code, reason = "wasm runs no checks: only the native worker and checks read or build this message"))]
pub(crate) enum KeyOutcome {
    /// The check ran; the key's warnings are exactly these (empty = clean).
    Checked(Vec<SettingWarning>),
    /// The check could not decide (config unreadable, I/O error): keep the previous entry.
    Skipped,
}

/// One queued check run: the generation stamped on each requested key, and the context
/// those keys must be checked with.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code, reason = "wasm runs no checks: only the native worker and checks read or build this message"))]
pub(crate) struct CheckRequest {
    generations: BTreeMap<SettingKey, u64>,
    ctx: CheckContext,
}

/// The outcomes of one run, each with the generation it answers.
#[derive(Debug)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code, reason = "wasm runs no checks: only the native worker and checks read or build this message"))]
pub(crate) struct CheckBatch {
    outcomes: Vec<(SettingKey, u64, KeyOutcome)>,
}

/// Owns the current [`WarningSet`] and the channels to the checks worker.
#[derive(Debug)]
pub struct SettingsWarnings {
    set: WarningSet,
    /// The latest generation requested per key; a result with another generation is stale.
    requested: BTreeMap<SettingKey, u64>,
    next_generation: u64,
    request_tx: Option<Sender<CheckRequest>>,
    result_rx: Option<Receiver<CheckBatch>>,
    /// The last explicit autostart value reported by the pane (`SettingChange::BackendAutostart`).
    /// It wins over the snapshot-derived `CheckContext::backend_autostart` of every later
    /// request: the pane's toggle is the only sender of `SetAutoStart` while this instance
    /// lives, so the latched value is never older than the supervisor snapshot, which only
    /// catches up once the supervisor worker dequeues the command.
    explicit_autostart: Option<bool>,
}

impl SettingsWarnings {
    /// An inert instance: empty set, no worker; `recheck` and `poll` do nothing.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            set: WarningSet::default(),
            requested: BTreeMap::new(),
            next_generation: 0,
            request_tx: None,
            result_rx: None,
            explicit_autostart: None,
        }
    }

    /// Spawns ONE long-lived `settings-warnings` worker and queues a full run of
    /// [`SettingKey::ALL`]. The worker calls `egui_ctx.request_repaint()` after each run.
    /// A spawn failure is logged and yields an inert instance.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn start(egui_ctx: &egui::Context, ctx: CheckContext) -> Self {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<CheckRequest>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<CheckBatch>();
        let repaint_ctx = egui_ctx.clone();
        let spawned = ms_thread::Builder::new()
            .name("settings-warnings".to_string())
            .spawn(move || {
                worker_loop(&request_rx, &result_tx, super::checks::run_checks, || repaint_ctx.request_repaint());
            });
        match spawned {
            // The worker is detached on purpose: it ends by itself when this instance (the
            // request sender) is dropped, and the GUI thread must never join it.
            Ok(_detached) => {}
            Err(err) => {
                runtime_log::log_error(format!(
                    "settings-warnings: could not spawn the checks worker; settings warnings are disabled for this session; error: {err}"
                ));
                return Self::disabled();
            }
        }
        let mut warnings =
            Self { request_tx: Some(request_tx), result_rx: Some(result_rx), ..Self::disabled() };
        warnings.queue(SettingKey::ALL, ctx);
        warnings
    }

    /// wasm: no checks run on the web build (no filesystem, backend or native ORT), so the
    /// instance is inert.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn start(_egui_ctx: &egui::Context, _ctx: CheckContext) -> Self {
        Self::disabled()
    }

    /// Queues the keys `changes` affect with a fresh generation. A
    /// `SettingChange::BackendAutostart(v)` is latched: `v` replaces `ctx.backend_autostart`
    /// for this and every later request (the supervisor snapshot that `ctx` was captured
    /// from lags the toggle). No-op on an inert instance.
    pub fn recheck(&mut self, changes: &[SettingChange], ctx: CheckContext) {
        let mut keys: Vec<SettingKey> = Vec::new();
        for change in changes {
            if let SettingChange::BackendAutostart(value) = change {
                self.explicit_autostart = Some(*value);
            }
            keys.extend_from_slice(change.affected_keys());
        }
        keys.sort_unstable();
        keys.dedup();
        if !keys.is_empty() {
            self.queue(&keys, ctx);
        }
    }

    /// Applies every result that arrived, dropping stale ones (generation older than the
    /// latest request of that key); `Skipped` keeps the previous entry. Non-blocking.
    /// Returns `true` when the set changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Some(rx) = self.result_rx.as_ref() {
            match rx.try_recv() {
                Ok(batch) => changed |= self.apply(batch),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    runtime_log::log_warn(
                        "settings-warnings: the checks worker stopped; the current warnings are kept but no longer refreshed",
                    );
                    self.result_rx = None;
                    self.request_tx = None;
                }
            }
        }
        changed
    }

    /// The current warnings.
    #[must_use]
    pub fn set(&self) -> &WarningSet {
        &self.set
    }

    /// Stamps `keys` with one fresh generation and sends them to the worker, with `ctx`
    /// corrected by the latched explicit autostart value.
    fn queue(&mut self, keys: &[SettingKey], ctx: CheckContext) {
        let Some(tx) = self.request_tx.as_ref() else {
            return;
        };
        let mut ctx = ctx;
        if let Some(value) = self.explicit_autostart {
            ctx.backend_autostart = value;
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        let generations: BTreeMap<SettingKey, u64> = keys.iter().map(|key| (*key, generation)).collect();
        match tx.send(CheckRequest { generations, ctx }) {
            Ok(()) => {
                for key in keys {
                    self.requested.insert(*key, generation);
                }
            }
            Err(_) => {
                runtime_log::log_warn(format!(
                    "settings-warnings: the checks worker is gone; recheck of {keys:?} dropped and checks stop for this session"
                ));
                self.request_tx = None;
                self.result_rx = None;
            }
        }
    }

    /// Applies one batch; returns `true` when the set changed.
    fn apply(&mut self, batch: CheckBatch) -> bool {
        let mut changed = false;
        for (key, generation, outcome) in batch.outcomes {
            if self.requested.get(&key) != Some(&generation) {
                continue;
            }
            if let KeyOutcome::Checked(warnings) = outcome
                && self.set.for_key(key) != warnings.as_slice()
            {
                self.set.replace_key(key, warnings);
                changed = true;
            }
        }
        changed
    }
}

/// Coalesces queued requests into evaluation groups: per key the max generation and the
/// context of the request that carried it, then the keys grouped by that context (in
/// first-seen order). A key is therefore never checked with another request's context.
#[cfg(not(target_arch = "wasm32"))]
fn coalesce(requests: impl IntoIterator<Item = CheckRequest>) -> Vec<(CheckContext, BTreeMap<SettingKey, u64>)> {
    let mut latest: BTreeMap<SettingKey, (u64, CheckContext)> = BTreeMap::new();
    for request in requests {
        for (key, generation) in request.generations {
            let slot = latest.entry(key).or_insert((generation, request.ctx));
            if generation >= slot.0 {
                *slot = (generation, request.ctx);
            }
        }
    }
    let mut groups: Vec<(CheckContext, BTreeMap<SettingKey, u64>)> = Vec::new();
    for (key, (generation, ctx)) in latest {
        match groups.iter_mut().find(|(group_ctx, _)| *group_ctx == ctx) {
            Some((_, keys)) => {
                keys.insert(key, generation);
            }
            None => groups.push((ctx, BTreeMap::from([(key, generation)]))),
        }
    }
    groups
}

/// The worker body: blocks for a request, coalesces everything queued behind it
/// ([`coalesce`]), runs `run` once per context group, sends ONE batch and calls
/// `on_delivered` (the repaint request). Returns when the request channel disconnects or
/// the result cannot be delivered. `run` is `checks::run_checks` in production; tests feed
/// a pure stand-in.
#[cfg(not(target_arch = "wasm32"))]
fn worker_loop(
    request_rx: &Receiver<CheckRequest>,
    result_tx: &Sender<CheckBatch>,
    mut run: impl FnMut(&BTreeSet<SettingKey>, CheckContext) -> Vec<(SettingKey, KeyOutcome)>,
    on_delivered: impl Fn(),
) {
    runtime_log::log_info("settings-warnings: worker started");
    while let Ok(first) = request_rx.recv() {
        let mut requests = vec![first];
        while let Ok(next) = request_rx.try_recv() {
            requests.push(next);
        }

        let mut batch = CheckBatch { outcomes: Vec::new() };
        for (ctx, generations) in coalesce(requests) {
            let keys: BTreeSet<SettingKey> = generations.keys().copied().collect();
            let started = std::time::Instant::now();
            runtime_log::log_info(format!("settings-warnings: run started; keys={keys:?}; context={ctx:?}"));
            let outcomes = run(&keys, ctx);
            log_run(&outcomes, started.elapsed());
            batch.outcomes.extend(
                outcomes
                    .into_iter()
                    .filter_map(|(key, outcome)| generations.get(&key).map(|generation| (key, *generation, outcome))),
            );
        }
        if result_tx.send(batch).is_err() {
            runtime_log::log_info("settings-warnings: the settings page is gone; worker exiting");
            return;
        }
        on_delivered();
    }
    runtime_log::log_info("settings-warnings: request channel closed; worker exiting");
}

/// Logs a finished run: every warning (English, `Debug` form) and a summary line.
#[cfg(not(target_arch = "wasm32"))]
fn log_run(outcomes: &[(SettingKey, KeyOutcome)], elapsed: std::time::Duration) {
    let mut warnings = 0_usize;
    let mut skipped = 0_usize;
    for (key, outcome) in outcomes {
        match outcome {
            KeyOutcome::Checked(list) => {
                for warning in list {
                    warnings += 1;
                    runtime_log::log_info(format!(
                        "settings-warnings: {key:?} [{:?}] {:?}",
                        warning.reason.level(),
                        warning.reason
                    ));
                }
            }
            KeyOutcome::Skipped => skipped += 1,
        }
    }
    runtime_log::log_info(format!(
        "settings-warnings: run finished; keys={}; warnings={warnings}; skipped={skipped}; elapsed_ms={}",
        outcomes.len(),
        elapsed.as_millis()
    ));
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, Sender, channel};

    use std::collections::{BTreeMap, BTreeSet};

    use super::{CheckBatch, CheckContext, CheckRequest, KeyOutcome, SettingsWarnings, coalesce, worker_loop};
    use crate::settings_warnings::model::{SettingChange, SettingKey, SettingWarning, WarningLevel, WarningReason};

    const CTX: CheckContext = CheckContext { ai_enabled: true, backend_autostart: true, registration_checks: true };

    /// A live instance whose worker side is the test: requests come out of the
    /// returned receiver, results go in through the returned sender.
    fn hand_fed() -> (SettingsWarnings, Receiver<CheckRequest>, Sender<CheckBatch>) {
        let (request_tx, request_rx) = channel();
        let (result_tx, result_rx) = channel();
        let warnings = SettingsWarnings {
            request_tx: Some(request_tx),
            result_rx: Some(result_rx),
            ..SettingsWarnings::disabled()
        };
        (warnings, request_rx, result_tx)
    }

    fn warning(key: SettingKey, tag: &str) -> SettingWarning {
        SettingWarning { key, reason: WarningReason::UiCatalogEnglish { tag: tag.to_string() } }
    }

    fn send(tx: &Sender<CheckBatch>, outcomes: Vec<(SettingKey, u64, KeyOutcome)>) {
        assert!(tx.send(CheckBatch { outcomes }).is_ok());
    }

    #[test]
    fn stale_full_run_does_not_overwrite_a_newer_recheck() {
        let (mut warnings, requests, results) = hand_fed();
        warnings.queue(SettingKey::ALL, CTX);
        warnings.recheck(&[SettingChange::UiLanguage], CTX);
        let full = requests.try_recv().map(|request| request.generations).unwrap_or_default();
        let recheck = requests.try_recv().map(|request| request.generations).unwrap_or_default();
        assert_eq!(full.len(), SettingKey::ALL.len());
        assert_eq!(full.get(&SettingKey::UiLanguage), Some(&1));
        assert_eq!(recheck.get(&SettingKey::UiLanguage), Some(&2));
        assert_eq!(recheck.len(), 1);

        // The newer recheck lands first, then the stale full-run answer for the same key.
        send(&results, vec![(SettingKey::UiLanguage, 2, KeyOutcome::Checked(vec![warning(SettingKey::UiLanguage, "new")]))]);
        send(
            &results,
            vec![
                (SettingKey::UiLanguage, 1, KeyOutcome::Checked(Vec::new())),
                (SettingKey::ProjectsRoot, 1, KeyOutcome::Checked(vec![warning(SettingKey::ProjectsRoot, "root")])),
            ],
        );
        assert!(warnings.poll());
        assert_eq!(warnings.set().for_key(SettingKey::UiLanguage), &[warning(SettingKey::UiLanguage, "new")]);
        // The full run is still the latest request for ProjectsRoot, so it applies.
        assert_eq!(warnings.set().item_level(SettingKey::ProjectsRoot), Some(WarningLevel::Yellow));
    }

    #[test]
    fn skipped_keeps_the_previous_entry_and_unchanged_reports_false() {
        let (mut warnings, _requests, results) = hand_fed();
        warnings.recheck(&[SettingChange::ProjectsRoot], CTX);
        let entry = vec![warning(SettingKey::ProjectsRoot, "root")];
        send(&results, vec![(SettingKey::ProjectsRoot, 1, KeyOutcome::Checked(entry.clone()))]);
        assert!(warnings.poll());
        warnings.recheck(&[SettingChange::ProjectsRoot], CTX);
        send(&results, vec![(SettingKey::ProjectsRoot, 2, KeyOutcome::Skipped)]);
        assert!(!warnings.poll());
        assert_eq!(warnings.set().for_key(SettingKey::ProjectsRoot), entry.as_slice());
        warnings.recheck(&[SettingChange::ProjectsRoot], CTX);
        send(&results, vec![(SettingKey::ProjectsRoot, 3, KeyOutcome::Checked(entry.clone()))]);
        assert!(!warnings.poll(), "an identical result is not a change");
        warnings.recheck(&[SettingChange::ProjectsRoot], CTX);
        send(&results, vec![(SettingKey::ProjectsRoot, 4, KeyOutcome::Checked(Vec::new()))]);
        assert!(warnings.poll());
        assert_eq!(warnings.set().overall_level(), None);
    }

    #[test]
    fn recheck_unions_keys_and_takes_the_autostart_value_of_the_change() {
        let (mut warnings, requests, _results) = hand_fed();
        warnings.recheck(&[SettingChange::BackendAutostart(false), SettingChange::AiInstallType], CTX);
        let request = requests.try_recv();
        assert!(request.is_ok());
        let Ok(request) = request else { return };
        assert_eq!(request.ctx, CheckContext { backend_autostart: false, ..CTX });
        let keys: Vec<SettingKey> = request.generations.keys().copied().collect();
        assert_eq!(keys, vec![SettingKey::AiRuntime, SettingKey::BackendAutostart]);
    }

    /// Each key keeps the context of the request that holds its max generation; keys with
    /// different contexts land in separate groups.
    #[test]
    fn coalesce_keeps_the_context_of_each_key_s_latest_request() {
        let on = CTX;
        let off = CheckContext { backend_autostart: false, ..CTX };
        let request = |ctx: CheckContext, generation: u64, keys: &[SettingKey]| CheckRequest {
            generations: keys.iter().map(|key| (*key, generation)).collect(),
            ctx,
        };
        let groups = coalesce([
            request(on, 1, &[SettingKey::BackendAutostart, SettingKey::OnnxBuild]),
            request(off, 2, &[SettingKey::BackendAutostart]),
            request(on, 3, &[SettingKey::OnnxBuild]),
        ]);
        assert_eq!(
            groups,
            vec![
                (off, BTreeMap::from([(SettingKey::BackendAutostart, 2)])),
                (on, BTreeMap::from([(SettingKey::OnnxBuild, 3)])),
            ]
        );
    }

    /// The reviewer's scenario on a real worker loop: the entry's full run is still queued,
    /// the user unchecks autostart, then a build save and an install-type reconcile land
    /// with contexts captured from a supervisor snapshot that still says "on". The
    /// autostart key must be checked with `false`, so no Red badge stays on the unchecked
    /// box.
    #[test]
    fn explicit_autostart_wins_over_later_stale_snapshot_contexts() {
        let (mut warnings, requests, results) = hand_fed();
        let stale = CTX;
        warnings.queue(SettingKey::ALL, stale);
        warnings.recheck(&[SettingChange::BackendAutostart(false)], stale);
        warnings.recheck(&[SettingChange::OnnxBuild], stale);
        warnings.recheck(&[SettingChange::AiInstallType], stale);
        // Dropping the sender lets the worker drain the four requests and exit.
        warnings.request_tx = None;

        let red = SettingWarning {
            key: SettingKey::BackendAutostart,
            reason: WarningReason::BackendScriptMissing { app_dir: "/home/u/ManhwaStudio".to_string() },
        };
        let mut contexts: Vec<(CheckContext, BTreeSet<SettingKey>)> = Vec::new();
        let delivered_cell = std::cell::Cell::new(0_u32);
        worker_loop(
            &requests,
            &results,
            |keys, ctx| {
                contexts.push((ctx, keys.clone()));
                keys.iter()
                    .map(|key| {
                        let flagged = *key == SettingKey::BackendAutostart && ctx.backend_autostart;
                        (*key, KeyOutcome::Checked(if flagged { vec![red.clone()] } else { Vec::new() }))
                    })
                    .collect()
            },
            || delivered_cell.set(delivered_cell.get() + 1),
        );
        assert_eq!(delivered_cell.get(), 1, "the four queued requests coalesce into one batch");
        // ProjectsRoot / UiLanguage still answer the entry's run, with its context; every
        // key requested after the toggle carries the latched value.
        let autostart_group = contexts.iter().find(|(_, keys)| keys.contains(&SettingKey::BackendAutostart));
        assert!(autostart_group.is_some_and(|(ctx, _)| !ctx.backend_autostart), "{contexts:?}");
        let native_group = contexts.iter().find(|(_, keys)| keys.contains(&SettingKey::OnnxBuild));
        assert!(native_group.is_some_and(|(ctx, _)| !ctx.backend_autostart), "{contexts:?}");
        assert!(!warnings.poll(), "every key is clean");
        assert_eq!(warnings.set().item_level(SettingKey::BackendAutostart), None);
    }

    #[test]
    fn disabled_instance_is_inert() {
        let mut warnings = SettingsWarnings::disabled();
        warnings.recheck(&[SettingChange::UiLanguage], CTX);
        assert!(!warnings.poll());
        assert_eq!(warnings.set().overall_level(), None);
        assert!(warnings.requested.is_empty());
    }

    #[test]
    fn worker_loss_is_detected_and_turns_inert() {
        let (mut warnings, requests, results) = hand_fed();
        drop(results);
        assert!(!warnings.poll());
        assert!(warnings.result_rx.is_none());
        drop(requests);
        warnings.recheck(&[SettingChange::UiLanguage], CTX);
        assert!(warnings.requested.is_empty());
    }
}
