/*
File: crates/ms-tab-translation/src/mt_limits/call.rs

Purpose:
What a machine-translation provider holds while it talks to its service: an `MtCall` (the
shared registry + the run's cancel flag + how long it may wait + who hears about waits), the
`MtPermit` it takes before EVERY HTTP request, and the typed `MtSignal` it finishes the permit
with.

Key items:
- `MtCall::for_run` (a Translation run: cancellable, waits up to `RUN_MAX_WAIT`, reports
  waits) and `MtCall::detached` (the Cleaning prompt translator: shared registry, no cancel,
  `DETACHED_MAX_WAIT`, silent).
- `MtCall::acquire` -> `MtPermit` or `MtAcquireError` (`Cancelled`, `CoolingDown`, `Busy`).
- `MtPermit::finish(MtSignal)`; dropping an unfinished permit (early return, panic) counts as
  `MtSignal::Neutral` and releases the slot.
- `MtWaitStatus` / `MtWaitReason`: what the wait reporter is told (`None` = the wait ended):
  a limiter wait inside `acquire`, or a browser tab that needs the user
  (`MtCall::report_attention`).

Notes:
- The acquire loop holds the registry lock only inside `MtLimiterRegistry::decide`; it sleeps
  in slices of `WAIT_SLICE` through the registry's clock, so Stop ends a wait within one slice.
- Waiters are not FIFO: whoever asks first after a slot frees gets it. With one run per
  service at a time this only matters for a run racing a detached caller.
- A wait is reported only when it is expected to last at least `REPORT_WAIT_MIN` AND longer
  than the lane's ordinary spacing (`min_interval`): the normal pace between requests stays
  silent, while a cooldown, a wait for a request-budget token, or a spacing wait stretched by
  queueing behind another request is shown. It is reported again only when the shown whole
  second (`whole_seconds_ceil`, the one rounding rule for every shown wait) changes.
- Attention reports (`report_attention`) come from a browser-extension provider while its
  request is in flight (permit held), so they never overlap an `acquire` wait of the same call;
  both use the same reporter, `Some` while shown and one `None` when it ends. A call dropped
  while attention is still shown ends it with `None`, so the panel never keeps a stale notice.
*/

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::limiter::Decision;
use super::registry::MtLimiterRegistry;
use super::table::{MtLane, MtRoute};
use crate::machine_translation::MtService;

/// Longest a Translation run waits for one request slot (spacing, a request-budget token, an
/// in-flight request of the lane, or a short cooldown) before it gives the request up.
pub(crate) const RUN_MAX_WAIT: Duration = Duration::from_secs(120);
/// Longest a detached caller (the Cleaning prompt translator, which cannot be stopped) waits.
pub(crate) const DETACHED_MAX_WAIT: Duration = Duration::from_secs(30);
/// Expected waits shorter than this are never reported, whatever the lane's spacing.
const REPORT_WAIT_MIN: Duration = Duration::from_millis(1500);
/// Longest single sleep of the acquire loop: bounds how late a cancel is noticed.
const WAIT_SLICE: Duration = Duration::from_millis(250);

/// Outcome of one HTTP request, as far as the limiter is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MtSignal {
    /// The service answered normally (whatever the translation's quality).
    Ok,
    /// The service said "too many requests" (HTTP 429, a provider rate-limit code).
    /// `retry_after` is the server's `Retry-After`, when it sent a usable one.
    RateLimited { retry_after: Option<Duration> },
    /// The service blocked this client (a captcha / "unusual traffic" page, a quota code).
    Blocked { retry_after: Option<Duration> },
    /// Anything that says nothing about the rate: network error, other HTTP status, bad
    /// format, consent page. Never changes the route's health.
    Neutral,
}

/// Why `MtCall::acquire` gave no permit. No request may be sent in any of these cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MtAcquireError {
    /// The run's cancel flag is set (Stop); the provider must stop sending.
    Cancelled,
    /// The route cools for longer than this call may wait; it is usable again in `retry_in`.
    CoolingDown { retry_in: Duration },
    /// Another request of the lane stayed in flight longer than this call may wait.
    Busy,
}

/// Why a call is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MtWaitReason {
    /// The limiter holds the request back (spacing, a request-budget token, an in-flight
    /// request of the lane, a cooldown being waited out) inside `MtCall::acquire`.
    Limiter,
    /// The browser tab driving the request needs the user (a captcha, a consent or sign-in
    /// page); the request waits until the user deals with it. Always `remaining: None`.
    BrowserAttention,
}

/// A wait in progress, as told to the call's reporter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MtWaitStatus {
    /// The service being waited for.
    pub(crate) service: MtService,
    /// Time left when known (spacing, cooldown); `None` = waiting for another in-flight
    /// request of the lane, or for the user, whose end is unknown.
    pub(crate) remaining: Option<Duration>,
    /// What the call waits for.
    pub(crate) reason: MtWaitReason,
}

/// `duration` in whole seconds, rounded up (2.1 s -> 3): how every wait is shown, so a
/// countdown never reads 0 while time is left.
#[must_use]
pub(crate) fn whole_seconds_ceil(duration: Duration) -> u64 {
    duration.as_secs().saturating_add(u64::from(duration.subsec_nanos() > 0))
}

/// The no-op reporter of a detached call.
fn ignore_wait(_status: Option<MtWaitStatus>) {}

/// A provider's handle on the limiter for one translation request batch.
pub(crate) struct MtCall<'a> {
    registry: &'a MtLimiterRegistry,
    cancel: Option<&'a AtomicBool>,
    max_wait: Duration,
    on_wait: &'a dyn Fn(Option<MtWaitStatus>),
    /// The service whose browser-attention notice is shown right now (`report_attention`).
    attention: Cell<Option<MtService>>,
}

impl std::fmt::Debug for MtCall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MtCall").field("cancelled", &self.is_cancelled()).field("max_wait", &self.max_wait).finish_non_exhaustive()
    }
}

impl<'a> MtCall<'a> {
    /// The call of a Translation run: waits up to `RUN_MAX_WAIT`, stops waiting (and makes the
    /// provider stop sending) as soon as `cancel` is set, and tells `on_wait` about every
    /// reportable wait (`Some`) and its end (`None`). `on_wait` runs on the worker thread with
    /// no lock held.
    #[must_use]
    pub(crate) fn for_run(registry: &'a MtLimiterRegistry, cancel: &'a AtomicBool, on_wait: &'a dyn Fn(Option<MtWaitStatus>)) -> Self {
        Self { registry, cancel: Some(cancel), max_wait: RUN_MAX_WAIT, on_wait, attention: Cell::new(None) }
    }

    /// Overrides how long this call may wait (tests of short waits and `Busy`).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Takes the request slot of `route`, waiting (in cancellable slices, reported) for
    /// spacing, a request-budget token, an in-flight request of the lane, or a cooldown
    /// short enough to wait out.
    /// The returned permit MUST be finished with the request's outcome.
    ///
    /// # Errors
    /// `Cancelled` when the cancel flag is (or becomes) set; `CoolingDown` when the route
    /// cools, or the next budget token is further away, than this call may wait (returned at
    /// once, without sleeping); `Busy`
    /// when another request of the lane outlasted the wait budget.
    pub(crate) fn acquire(&self, route: MtRoute) -> Result<MtPermit<'a>, MtAcquireError> {
        let clock = self.registry.clock();
        let started = clock.now();
        let lane = route.lane();
        let mut reporter = WaitReporter { on_wait: self.on_wait, service: lane.service, ordinary_spacing: self.registry.min_interval(lane), last_shown: None };
        let result = loop {
            if self.is_cancelled() {
                break Err(MtAcquireError::Cancelled);
            }
            let waited = clock.now().saturating_duration_since(started);
            let budget = self.max_wait.saturating_sub(waited);
            match self.registry.decide(route, budget) {
                Decision::Admit => break Ok(MtPermit { registry: self.registry, route, signal: MtSignal::Neutral }),
                Decision::CoolingDown { retry_in } => break Err(MtAcquireError::CoolingDown { retry_in }),
                Decision::Wait { remaining } => {
                    // Spacing, token and cooldown waits are bounded by `decide`; only a wait on
                    // an in-flight request can grow without limit.
                    if remaining.is_none() && budget.is_zero() {
                        break Err(MtAcquireError::Busy);
                    }
                    reporter.waiting(waited, remaining);
                    clock.sleep(remaining.map_or(WAIT_SLICE, |left| left.min(WAIT_SLICE)));
                }
            }
        };
        reporter.finish();
        result
    }

    /// Whether the run was stopped. A provider checks it between texts and sends nothing more
    /// once it is set. Always `false` for a detached call.
    #[must_use]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }

    /// `Some(time until the lane is usable again)` when every route of `lane` is cooling;
    /// `None` otherwise.
    #[must_use]
    pub(crate) fn lane_cooldown(&self, lane: MtLane) -> Option<Duration> {
        self.registry.lane_cooldown(lane)
    }

    /// Tells the wait reporter that the browser tab driving the current request of `service`
    /// needs the user (`active`: a captcha or consent page appeared) or no longer does. Reports
    /// only changes: the first `true` sends `Some(MtWaitStatus { reason: BrowserAttention,
    /// remaining: None, .. })`, the next `false` sends `None`; repeats are silent. Called by a
    /// browser-extension provider while it holds a permit, on the worker thread, no lock held.
    pub(crate) fn report_attention(&self, service: MtService, active: bool) {
        let shown = if active { Some(service) } else { None };
        if self.attention.replace(shown) == shown {
            return;
        }
        (self.on_wait)(shown.map(|service| MtWaitStatus { service, remaining: None, reason: MtWaitReason::BrowserAttention }));
    }
}

impl Drop for MtCall<'_> {
    /// Ends a browser-attention notice that is still shown (a provider that returned early).
    fn drop(&mut self) {
        if self.attention.take().is_some() {
            (self.on_wait)(None);
        }
    }
}

impl MtCall<'static> {
    /// The call of a caller that cannot be stopped and shows no wait status (the Cleaning
    /// prompt translator): the shared registry, no cancel, `DETACHED_MAX_WAIT`.
    #[must_use]
    pub(crate) fn detached() -> Self {
        Self { registry: MtLimiterRegistry::shared(), cancel: None, max_wait: DETACHED_MAX_WAIT, on_wait: &ignore_wait, attention: Cell::new(None) }
    }
}

/// The right to send ONE request on a route. Finish it with the request's outcome; dropping
/// it unfinished records `MtSignal::Neutral` (the slot is always released).
#[must_use = "a permit is the right to send one request; finish it with the request's MtSignal"]
#[derive(Debug)]
pub(crate) struct MtPermit<'a> {
    registry: &'a MtLimiterRegistry,
    route: MtRoute,
    signal: MtSignal,
}

impl MtPermit<'_> {
    /// Releases the slot and records the request's outcome.
    pub(crate) fn finish(mut self, signal: MtSignal) {
        self.signal = signal;
        // `Drop` reports `self.signal` to the registry.
    }
}

impl Drop for MtPermit<'_> {
    fn drop(&mut self) {
        self.registry.finish(self.route, self.signal);
    }
}

/// Reports the waits of one acquire: only long ones (at least `REPORT_WAIT_MIN` and longer
/// than `ordinary_spacing`), once per shown whole second, and a final `None` when anything
/// was reported.
struct WaitReporter<'r> {
    on_wait: &'r dyn Fn(Option<MtWaitStatus>),
    service: MtService,
    /// The lane's normal start-to-start spacing; a wait no longer than it is the regular
    /// pace, not something to show.
    ordinary_spacing: Duration,
    /// The whole seconds last shown (`Some(None)` = shown as "unknown"); `None` = nothing shown.
    last_shown: Option<Option<u64>>,
}

impl WaitReporter<'_> {
    /// One wait iteration: `waited` so far, `remaining` as decided.
    fn waiting(&mut self, waited: Duration, remaining: Option<Duration>) {
        let expected_total = remaining.map_or(waited, |left| waited + left);
        if self.last_shown.is_none() && (expected_total < REPORT_WAIT_MIN || expected_total <= self.ordinary_spacing) {
            return;
        }
        let shown = remaining.map(whole_seconds_ceil);
        if self.last_shown != Some(shown) {
            self.last_shown = Some(shown);
            (self.on_wait)(Some(MtWaitStatus { service: self.service, remaining, reason: MtWaitReason::Limiter }));
        }
    }

    /// Ends the reported wait, if any.
    fn finish(&self) {
        if self.last_shown.is_some() {
            (self.on_wait)(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use web_time::Instant;

    use super::*;
    use crate::mt_limits::table::{LaneLimits, LimitsTable, RouteLimits};
    use crate::mt_limits::test_support::FakeClock;
    use crate::mt_limits::registry::{MtClock, SystemClock};

    fn shown_seconds(log: &[Option<MtWaitStatus>]) -> Vec<Option<u64>> {
        log.iter().map(|status| status.and_then(|s| s.remaining).map(whole_seconds_ceil)).collect()
    }

    #[test]
    fn yandex_cooldown_is_waited_reported_and_ended() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        call.acquire(MtRoute::Yandex).map(|permit| permit.finish(MtSignal::RateLimited { retry_after: None })).unwrap_or_else(|err| panic!("first acquire: {err:?}"));
        let before = clock.now();
        let permit = call.acquire(MtRoute::Yandex).unwrap_or_else(|err| panic!("cooldown must be waited out: {err:?}"));
        assert_eq!(clock.now() - before, Duration::from_secs(30));
        permit.finish(MtSignal::Ok);
        let expected: Vec<Option<u64>> = (1..=30).rev().map(Some).chain([None]).collect();
        assert_eq!(shown_seconds(&log.borrow()), expected);
        assert!(log.borrow().iter().flatten().all(|status| status.service == MtService::Yandex));
        assert!(clock.sleeps().iter().all(|&slice| slice <= WAIT_SLICE));
    }

    #[test]
    fn ordinary_spacing_is_not_reported() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        for _ in 0..5 {
            call.acquire(MtRoute::Yandex).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        }
        assert!(log.borrow().is_empty());
        assert_eq!(clock.sleeps().iter().sum::<Duration>(), Duration::from_secs(4));
    }

    #[test]
    fn google_cooling_route_is_refused_without_sleeping() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        call.acquire(MtRoute::GoogleGtx).map(|permit| permit.finish(MtSignal::Blocked { retry_after: None })).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.acquire(MtRoute::GoogleGtx).err(), Some(MtAcquireError::CoolingDown { retry_in: Duration::from_secs(300) }));
        assert!(clock.sleeps().is_empty());
        assert_eq!(call.lane_cooldown(MtLane::builtin(MtService::Google)), None);
        call.acquire(MtRoute::GoogleClients5).map(|permit| permit.finish(MtSignal::RateLimited { retry_after: Some(Duration::from_secs(600)) })).unwrap_or_else(|err| panic!("{err:?}"));
        // clients5 waited 1 s of spacing first, so gtx (5 min) ends before clients5 (10 min).
        assert_eq!(call.lane_cooldown(MtLane::builtin(MtService::Google)), Some(Duration::from_secs(299)));
        // Once the gtx cooldown is over, its probe is admitted again.
        clock.advance(Duration::from_secs(299));
        call.acquire(MtRoute::GoogleGtx).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
    }

    #[test]
    fn cancel_during_a_wait_returns_within_one_slice() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let cancelled_at = RefCell::new(None);
        let on_wait = |status: Option<MtWaitStatus>| {
            if status.is_some() && cancelled_at.borrow().is_none() {
                cancel.store(true, Ordering::Relaxed);
                *cancelled_at.borrow_mut() = Some(clock.now());
            }
        };
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        call.acquire(MtRoute::Yandex).map(|permit| permit.finish(MtSignal::RateLimited { retry_after: None })).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.acquire(MtRoute::Yandex).err(), Some(MtAcquireError::Cancelled));
        let Some(at) = *cancelled_at.borrow() else { panic!("the 30 s Yandex cooldown must be reported") };
        assert!(clock.now() - at <= WAIT_SLICE);
        assert!(call.is_cancelled());
        // Nothing was admitted: the route is still cooling.
        assert!(matches!(registry.decide(MtRoute::Yandex, Duration::ZERO), Decision::CoolingDown { .. }));
    }

    #[test]
    fn cancelled_call_never_admits() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(true);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        assert_eq!(call.acquire(MtRoute::Deepl).err(), Some(MtAcquireError::Cancelled));
        assert_eq!(registry.decide(MtRoute::Deepl, Duration::ZERO), Decision::Admit);
    }

    #[test]
    fn in_flight_wait_reports_unknown_time_and_ends_busy() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait).with_max_wait(Duration::from_secs(2));
        let held = call.acquire(MtRoute::Yandex).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.acquire(MtRoute::Yandex).err(), Some(MtAcquireError::Busy));
        assert_eq!(*log.borrow(), [Some(MtWaitStatus { service: MtService::Yandex, remaining: None, reason: MtWaitReason::Limiter }), None]);
        held.finish(MtSignal::Ok);
    }

    #[test]
    fn deepl_budget_wait_is_waited_reported_and_ended() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let call = MtCall::for_run(&registry, &cancel, &on_wait);
        // A full bucket: six requests back to back, none of them waits or is reported.
        for _ in 0..6 {
            call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        }
        assert!(clock.sleeps().is_empty());
        assert!(log.borrow().is_empty());
        // The seventh waits the minute until the next token, shown second by second.
        let before = clock.now();
        call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(clock.now() - before, Duration::from_secs(60));
        let expected: Vec<Option<u64>> = (1..=60).rev().map(Some).chain([None]).collect();
        assert_eq!(shown_seconds(&log.borrow()), expected);
        assert!(clock.sleeps().iter().all(|&slice| slice <= WAIT_SLICE));
    }

    #[test]
    fn cancel_during_a_budget_wait_admits_nothing() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        for _ in 0..6 {
            call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        }
        let cancelled_at = RefCell::new(None);
        let on_wait = |status: Option<MtWaitStatus>| {
            if status.and_then(|status| status.remaining).is_some_and(|left| left <= Duration::from_secs(40)) && cancelled_at.borrow().is_none() {
                cancel.store(true, Ordering::Relaxed);
                *cancelled_at.borrow_mut() = Some(clock.now());
            }
        };
        let stopping = MtCall::for_run(&registry, &cancel, &on_wait);
        assert_eq!(stopping.acquire(MtRoute::Deepl).err(), Some(MtAcquireError::Cancelled));
        let Some(at) = *cancelled_at.borrow() else { panic!("the token wait must be reported") };
        assert!(clock.now() - at <= WAIT_SLICE);
        // No token was taken: a fresh call still owes the rest of the minute.
        let left = match registry.decide(MtRoute::Deepl, Duration::from_secs(120)) {
            Decision::Wait { remaining: Some(left) } => left,
            other => panic!("expected a token wait, got {other:?}"),
        };
        assert!(left > Duration::ZERO && left <= Duration::from_secs(40));
    }

    #[test]
    fn a_detached_budget_refuses_a_token_wait_it_cannot_afford() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait).with_max_wait(DETACHED_MAX_WAIT);
        for _ in 0..6 {
            call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        }
        assert_eq!(call.acquire(MtRoute::Deepl).err(), Some(MtAcquireError::CoolingDown { retry_in: Duration::from_secs(60) }));
        assert!(clock.sleeps().is_empty());
        // Once the remaining wait fits the budget, it is waited out.
        clock.advance(Duration::from_secs(40));
        call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(clock.sleeps().iter().sum::<Duration>(), Duration::from_secs(20));
    }

    #[test]
    fn whole_seconds_round_up() {
        assert_eq!(whole_seconds_ceil(Duration::ZERO), 0);
        assert_eq!(whole_seconds_ceil(Duration::from_millis(1)), 1);
        assert_eq!(whole_seconds_ceil(Duration::from_secs(4)), 4);
        assert_eq!(whole_seconds_ceil(Duration::from_millis(2100)), 3);
        assert_eq!(whole_seconds_ceil(Duration::MAX), u64::MAX);
    }

    #[test]
    fn dropped_permit_releases_the_slot() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait).with_max_wait(Duration::ZERO);
        drop(call.acquire(MtRoute::Deepl).unwrap_or_else(|err| panic!("{err:?}")));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Held (never finished) so that the unwind drops it, like a provider panicking
            // mid-request.
            let _permit = call.acquire(MtRoute::Deepl);
            panic!("provider panics mid-request");
        }));
        assert!(panicked.is_err());
        // Both permits were released as Neutral: the next one waits only for spacing.
        let permit = call.acquire(MtRoute::Deepl).unwrap_or_else(|err| panic!("{err:?}"));
        permit.finish(MtSignal::Ok);
        assert!(!registry.is_locked());
    }

    #[test]
    fn detached_call_is_uncancellable_on_the_shared_registry() {
        let call = MtCall::detached();
        assert!(!call.is_cancelled());
        assert!(std::ptr::eq(call.registry, MtLimiterRegistry::shared()));
        assert_eq!(call.max_wait, DETACHED_MAX_WAIT);
        let cancel = AtomicBool::new(false);
        assert_eq!(MtCall::for_run(MtLimiterRegistry::shared(), &cancel, &ignore_wait).max_wait, RUN_MAX_WAIT);
    }

    #[test]
    fn a_builtin_cooldown_never_blocks_the_browser_lane_and_vice_versa() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait);
        call.acquire(MtRoute::Deepl).map(|permit| permit.finish(MtSignal::RateLimited { retry_after: None })).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.lane_cooldown(MtLane::builtin(MtService::Deepl)), Some(Duration::from_secs(120)));
        assert_eq!(call.lane_cooldown(MtLane::browser(MtService::Deepl)), None);
        // The browser lane admits at once: its own slot, spacing and health.
        call.acquire(MtRoute::DeeplBrowser).map(|permit| permit.finish(MtSignal::Blocked { retry_after: None })).unwrap_or_else(|err| panic!("{err:?}"));
        assert!(clock.sleeps().is_empty());
        // A browser cooldown (1 min) is refused at once, never waited out in place.
        assert_eq!(call.acquire(MtRoute::DeeplBrowser).err(), Some(MtAcquireError::CoolingDown { retry_in: Duration::from_secs(60) }));
        assert!(clock.sleeps().is_empty());
        // The browser cooldown ends first; the builtin lane still has a minute left.
        clock.advance(Duration::from_secs(60));
        call.acquire(MtRoute::DeeplBrowser).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.acquire(MtRoute::Deepl).err(), Some(MtAcquireError::CoolingDown { retry_in: Duration::from_secs(60) }));
        // A browser limit hit leaves a healthy builtin lane alone.
        clock.advance(Duration::from_secs(60));
        call.acquire(MtRoute::GoogleBrowser).map(|permit| permit.finish(MtSignal::Blocked { retry_after: None })).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.lane_cooldown(MtLane::browser(MtService::Google)), Some(Duration::from_secs(60)));
        assert_eq!(call.lane_cooldown(MtLane::builtin(MtService::Google)), None);
        call.acquire(MtRoute::GoogleGtx).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
    }

    #[test]
    fn lanes_of_one_service_have_separate_in_flight_slots() {
        let (clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let call = MtCall::for_run(&registry, &cancel, &ignore_wait).with_max_wait(Duration::ZERO);
        let builtin = call.acquire(MtRoute::Yandex).unwrap_or_else(|err| panic!("{err:?}"));
        let browser = call.acquire(MtRoute::YandexBrowser).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(call.acquire(MtRoute::YandexBrowser).err(), Some(MtAcquireError::Busy));
        builtin.finish(MtSignal::Ok);
        browser.finish(MtSignal::Ok);
        // The browser lane spaces its own requests 2 s apart.
        let spaced = MtCall::for_run(&registry, &cancel, &ignore_wait);
        spaced.acquire(MtRoute::YandexBrowser).map(|permit| permit.finish(MtSignal::Ok)).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(clock.sleeps().iter().sum::<Duration>(), Duration::from_secs(2));
    }

    #[test]
    fn browser_attention_is_reported_once_per_change_and_ended() {
        let (_clock, registry) = FakeClock::registry();
        let cancel = AtomicBool::new(false);
        let log = RefCell::new(Vec::new());
        let on_wait = |status: Option<MtWaitStatus>| log.borrow_mut().push(status);
        let attention = Some(MtWaitStatus { service: MtService::Deepl, remaining: None, reason: MtWaitReason::BrowserAttention });
        {
            let call = MtCall::for_run(&registry, &cancel, &on_wait);
            let permit = call.acquire(MtRoute::DeeplBrowser).unwrap_or_else(|err| panic!("{err:?}"));
            call.report_attention(MtService::Deepl, false);
            call.report_attention(MtService::Deepl, true);
            call.report_attention(MtService::Deepl, true);
            call.report_attention(MtService::Deepl, false);
            call.report_attention(MtService::Deepl, false);
            assert_eq!(*log.borrow(), [attention, None]);
            // Shown again, then the provider returns without clearing it.
            call.report_attention(MtService::Deepl, true);
            permit.finish(MtSignal::Ok);
            assert_eq!(log.borrow().len(), 3);
        }
        // Dropping the call ended the stale notice.
        assert_eq!(*log.borrow(), [attention, None, attention, None]);
    }

    fn fast_lane(_lane: MtLane) -> LaneLimits {
        LaneLimits {
            max_in_flight: 1,
            min_interval: Duration::from_millis(1),
            burst: 2,
            burst_idle: Duration::from_millis(1),
            request_budget: None,
            max_request_chars: None,
            rate_limit_retries: 0,
            retry_after_cap: Duration::from_millis(5),
        }
    }

    const FAST_STEPS: &[Duration] = &[Duration::from_millis(2)];

    fn fast_route(_route: MtRoute) -> RouteLimits {
        RouteLimits {
            cooldown_steps: FAST_STEPS,
            jitter: (Duration::ZERO, Duration::from_millis(1)),
            max_cooldown_wait: Duration::from_secs(1),
            recovery_successes: 1,
            level_memory: Duration::ZERO,
        }
    }

    #[test]
    fn two_threads_never_hold_two_permits_of_one_service() {
        let registry = MtLimiterRegistry::with_table(Arc::new(SystemClock), LimitsTable { lane: fast_lane, route: fast_route });
        let in_flight = AtomicU32::new(0);
        let started = Instant::now();
        ms_thread::scope(|scope| {
            for worker in 0..2_u32 {
                let (registry, in_flight) = (&registry, &in_flight);
                scope.spawn(move || {
                    let cancel = AtomicBool::new(false);
                    let call = MtCall::for_run(registry, &cancel, &ignore_wait);
                    for request in 0..20_u32 {
                        let permit = call.acquire(MtRoute::Yandex).unwrap_or_else(|err| panic!("worker {worker}: {err:?}"));
                        assert_eq!(in_flight.fetch_add(1, Ordering::SeqCst), 0, "two permits in flight");
                        SystemClock.sleep(Duration::from_micros(300));
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        let signal = if request % 7 == 3 { MtSignal::RateLimited { retry_after: None } } else { MtSignal::Ok };
                        permit.finish(signal);
                    }
                });
            }
        });
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        assert!(started.elapsed() < Duration::from_secs(20));
    }
}
