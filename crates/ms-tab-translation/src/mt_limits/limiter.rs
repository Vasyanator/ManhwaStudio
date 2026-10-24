/*
File: crates/ms-tab-translation/src/mt_limits/limiter.rs

Purpose:
The pure decision core of the machine-translation limiter: the pacing and cooldown state of
ONE lane (a service over one transport, `table::MtLane`) and the two transitions that change
it (`decide` before a request, `finish` after it). No clock, no lock, no sleep, no I/O: the
caller passes `now` and the jitter in, which makes every rule testable with plain `Instant`
arithmetic.

Key items:
- `LaneState`: in-flight count, burst budget, request-budget bucket, last start / end,
  per-route health.
- `TokenBucket`: the integer token count of a `RequestBudget` and the instant the next token
  accrues from.
- `RouteHealth`: `Healthy` -> `Cooling` -> `Probation` -> `Healthy` (or back to `Cooling`).
- `Decision`: `Admit` (the request is already counted as started), `Wait`, `CoolingDown`.

Rules (the table numbers come from `table.rs`):
1. Admit when `in_flight < max_in_flight`, the route is not cooling, and either a burst
   request is left or `min_interval` passed since the last START. The burst refills only when
   the lane is idle for `burst_idle` since the last END and no route cools or is in
   probation (a throttled lane runs with strict spacing).
   A lane with a `request_budget` also needs a token: an empty bucket makes the request wait
   for the next token (`Wait` with the exact time left), or, when that is longer than the
   caller may wait, answers `CoolingDown` with that time. Admission takes one token.
2. A limit signal (`RateLimited` / `Blocked`) cools the route for
   `max(step[level] + jitter, min(retry_after, retry_after_cap))`. `level` continues the
   previous progression if the route recovered less than `level_memory` ago, else restarts.
3. When the cooldown ends, the route enters `Probation`; `recovery_successes` successes make it
   healthy, a limit signal cools it again one step further (clamped to the last step).
4. `Neutral` signals (network, other HTTP errors, bad format) never change health. A limit
   signal also empties the lane's bucket: the server just said the budget is spent, so
   tokens accrue again from that moment.
5. A cooling route is waited out only when its remaining time fits
   `min(max_cooldown_wait, caller budget)`; otherwise the decision is `CoolingDown` at once.
*/

use std::time::Duration;

use web_time::Instant;

use super::call::MtSignal;
use super::table::{LaneLimits, LimitsTable, MtLane, MtRoute, RequestBudget, RouteLimits, routes_of};

/// Health of one route of a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteHealth {
    /// Requests run at the lane pace. `restored_at` is when the route last left
    /// `Probation` (`None` = never throttled), `level` the cooldown step it recovered from.
    Healthy { restored_at: Option<Instant>, level: usize },
    /// No request until `until`; `level` indexes `RouteLimits::cooldown_steps`.
    Cooling { until: Instant, level: usize },
    /// The cooldown ended; `successes` probes succeeded since.
    Probation { level: usize, successes: u32 },
}

/// Answer of `LaneState::decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Start the request now; it is already counted as in flight.
    Admit,
    /// Ask again later. `remaining` is the known time left (spacing or a cooldown being
    /// waited out); `None` = waiting for an in-flight request of the lane to end.
    Wait { remaining: Option<Duration> },
    /// The route cools for longer than the caller may wait; send nothing.
    CoolingDown { retry_in: Duration },
}

/// One route's rules and current health.
#[derive(Debug, Clone, Copy)]
struct RouteState {
    route: MtRoute,
    limits: RouteLimits,
    health: RouteHealth,
}

/// The token count of a `RequestBudget`. Tokens are whole numbers; the fraction of the next
/// token is kept as the instant it accrues from (`anchor`), so refills are exact on any clock.
#[derive(Debug, Clone, Copy)]
struct TokenBucket {
    budget: RequestBudget,
    tokens: u32,
    /// When the next token started accruing; `None` = never used (the bucket is full). While
    /// the bucket is full it is moved along with `now`, so idleness never stores extra tokens.
    anchor: Option<Instant>,
}

impl TokenBucket {
    /// A full bucket of `budget`.
    fn new(budget: RequestBudget) -> Self {
        Self { budget, tokens: budget.capacity, anchor: None }
    }

    /// Adds every whole token accrued up to `now` (never above the capacity).
    fn refill(&mut self, now: Instant) {
        let Some(anchor) = self.anchor else {
            self.anchor = Some(now);
            return;
        };
        if self.tokens >= self.budget.capacity {
            self.anchor = Some(now);
            return;
        }
        let period_nanos = self.budget.refill_every.as_nanos().max(1);
        let gained = now.saturating_duration_since(anchor).as_nanos() / period_nanos;
        let missing = self.budget.capacity.saturating_sub(self.tokens);
        // `gained` is compared in u128 so a huge idle span cannot wrap; the result is at most
        // `missing`, a u32.
        let added = u32::try_from(gained.min(u128::from(missing))).unwrap_or(missing);
        self.tokens = self.tokens.saturating_add(added);
        self.anchor = Some(if self.tokens >= self.budget.capacity { now } else { anchor + self.budget.refill_every.saturating_mul(added) });
    }

    /// Time until the next token at `now` (zero when one is available). Call after `refill`.
    fn wait_for_token(&self, now: Instant) -> Duration {
        if self.tokens > 0 {
            return Duration::ZERO;
        }
        self.anchor.map_or(Duration::ZERO, |anchor| (anchor + self.budget.refill_every).saturating_duration_since(now))
    }

    /// Takes the token of an admitted request. Call only when `wait_for_token` is zero.
    fn take(&mut self) {
        self.tokens = self.tokens.saturating_sub(1);
    }

    /// Empties the bucket at `now` (the server reported the budget spent).
    fn drain(&mut self, now: Instant) {
        self.tokens = 0;
        self.anchor = Some(now);
    }
}

/// Pacing and cooldown state of one lane (all of its routes).
#[derive(Debug, Clone)]
pub(crate) struct LaneState {
    limits: LaneLimits,
    routes: Vec<RouteState>,
    in_flight: u32,
    burst_left: u32,
    /// The request budget's bucket; `None` for a lane without one.
    bucket: Option<TokenBucket>,
    last_start: Option<Instant>,
    last_end: Option<Instant>,
}

impl LaneState {
    /// Fresh state of `lane`: every route healthy, full burst, nothing in flight.
    #[must_use]
    pub(crate) fn new(lane: MtLane, table: LimitsTable) -> Self {
        let limits = (table.lane)(lane);
        let routes = routes_of(lane)
            .iter()
            .map(|&route| RouteState { route, limits: (table.route)(route), health: RouteHealth::Healthy { restored_at: None, level: 0 } })
            .collect();
        Self { limits, routes, in_flight: 0, burst_left: limits.burst, bucket: limits.request_budget.map(TokenBucket::new), last_start: None, last_end: None }
    }

    /// Current health of `route` (`None` when the route is not one of this lane's).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn route_health(&self, route: MtRoute) -> Option<RouteHealth> {
        self.routes.iter().find(|state| state.route == route).map(|state| state.health)
    }

    /// Decides whether a request on `route` may start at `now`. `max_wait` is how much longer
    /// the caller is prepared to wait; it bounds which cooldowns are waited out (rule 5).
    /// `Admit` counts the request as started and takes its budget token. The registry keys states by
    /// `MtRoute::lane`, so `route` always belongs to this lane.
    pub(crate) fn decide(&mut self, route: MtRoute, now: Instant, max_wait: Duration) -> Decision {
        self.end_finished_cooldowns(now);
        if let Some(state) = self.routes.iter().find(|state| state.route == route)
            && let RouteHealth::Cooling { until, .. } = state.health
        {
            let remaining = until.saturating_duration_since(now);
            return if remaining <= state.limits.max_cooldown_wait.min(max_wait) { Decision::Wait { remaining: Some(remaining) } } else { Decision::CoolingDown { retry_in: remaining } };
        }
        if self.in_flight >= self.limits.max_in_flight {
            return Decision::Wait { remaining: None };
        }
        self.refill_burst(now);
        let spacing_left = self.last_start.map_or(Duration::ZERO, |start| (start + self.limits.min_interval).saturating_duration_since(now));
        let spacing_left = if self.burst_left == 0 { spacing_left } else { Duration::ZERO };
        let token_left = self.bucket.as_mut().map_or(Duration::ZERO, |bucket| {
            bucket.refill(now);
            bucket.wait_for_token(now)
        });
        // A token wait beyond the caller's budget is refused like a long cooldown, so a caller
        // with a short budget (the detached prompt translator) does not sit out a full period.
        if token_left > max_wait {
            return Decision::CoolingDown { retry_in: token_left };
        }
        let left = spacing_left.max(token_left);
        if !left.is_zero() {
            return Decision::Wait { remaining: Some(left) };
        }
        if let Some(bucket) = self.bucket.as_mut() {
            bucket.take();
        }
        self.burst_left = self.burst_left.saturating_sub(1);
        self.in_flight = self.in_flight.saturating_add(1);
        self.last_start = Some(now);
        Decision::Admit
    }

    /// Records the end of a request admitted on `route` with its outcome `signal`. `jitter` is
    /// added to a new cooldown step (drawn by the caller from the route's jitter range).
    /// Returns the cooldown the route entered, if this signal started or extended one.
    pub(crate) fn finish(&mut self, route: MtRoute, now: Instant, signal: MtSignal, jitter: Duration) -> Option<Duration> {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.last_end = Some(now);
        let retry_after_cap = self.limits.retry_after_cap;
        let retry_after = match signal {
            MtSignal::Ok => {
                let state = self.routes.iter_mut().find(|state| state.route == route)?;
                state.health = after_success(state.health, now, state.limits);
                return None;
            }
            MtSignal::Neutral => return None,
            MtSignal::RateLimited { retry_after } | MtSignal::Blocked { retry_after } => retry_after,
        };
        // The server says the budget is spent, whatever this process counted (rule 4).
        if let Some(bucket) = self.bucket.as_mut() {
            bucket.drain(now);
        }
        let state = self.routes.iter_mut().find(|state| state.route == route)?;
        let last_step = state.limits.cooldown_steps.len().saturating_sub(1);
        let level = match state.health {
            RouteHealth::Healthy { restored_at, level } => {
                let continues = restored_at.is_some_and(|restored| now.saturating_duration_since(restored) < state.limits.level_memory);
                if continues { (level + 1).min(last_step) } else { 0 }
            }
            RouteHealth::Probation { level, .. } => (level + 1).min(last_step),
            // A request admitted before the cooldown began (another route of the same
            // lane, or a probe racing the transition) reports a limit again: keep the
            // level, only extend the time.
            RouteHealth::Cooling { level, .. } => level,
        };
        let step = state.limits.cooldown_steps.get(level).copied().unwrap_or(Duration::ZERO);
        let server_wait = retry_after.map_or(Duration::ZERO, |wait| wait.min(retry_after_cap));
        let cooldown = (step + jitter).max(server_wait);
        let mut until = now + cooldown;
        if let RouteHealth::Cooling { until: previous, .. } = state.health {
            until = until.max(previous);
        }
        state.health = RouteHealth::Cooling { until, level };
        // A throttled lane runs with strict spacing until every route is healthy (rule 1).
        self.burst_left = 0;
        Some(until.saturating_duration_since(now))
    }

    /// `Some(shortest remaining cooldown)` when EVERY route of the lane is cooling at
    /// `now`, i.e. no request of the lane can be sent; `None` otherwise.
    #[must_use]
    pub(crate) fn lane_cooldown(&self, now: Instant) -> Option<Duration> {
        self.routes
            .iter()
            .map(|state| match state.health {
                RouteHealth::Cooling { until, .. } if until > now => Some(until.saturating_duration_since(now)),
                RouteHealth::Cooling { .. } | RouteHealth::Healthy { .. } | RouteHealth::Probation { .. } => None,
            })
            .collect::<Option<Vec<Duration>>>()
            .and_then(|remaining| remaining.into_iter().min())
    }

    /// Moves every route whose cooldown ended into `Probation` (rule 3).
    fn end_finished_cooldowns(&mut self, now: Instant) {
        for state in &mut self.routes {
            if let RouteHealth::Cooling { until, level } = state.health
                && until <= now
            {
                state.health = RouteHealth::Probation { level, successes: 0 };
            }
        }
    }

    /// Grants the burst again after `burst_idle` of idleness, unless a route of the lane is
    /// still throttled (rule 1).
    fn refill_burst(&mut self, now: Instant) {
        let throttled = self.routes.iter().any(|state| match state.health {
            RouteHealth::Cooling { .. } | RouteHealth::Probation { .. } => true,
            RouteHealth::Healthy { .. } => false,
        });
        let idle = self.in_flight == 0 && self.last_end.is_none_or(|end| now.saturating_duration_since(end) >= self.limits.burst_idle);
        if idle && !throttled {
            self.burst_left = self.limits.burst;
        }
    }
}

/// Health after a successful request on a route in `health` (rule 3).
fn after_success(health: RouteHealth, now: Instant, limits: RouteLimits) -> RouteHealth {
    match health {
        RouteHealth::Probation { level, successes } => {
            let successes = successes.saturating_add(1);
            if successes >= limits.recovery_successes { RouteHealth::Healthy { restored_at: Some(now), level } } else { RouteHealth::Probation { level, successes } }
        }
        RouteHealth::Healthy { .. } | RouteHealth::Cooling { .. } => health,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine_translation::MtService;

    const MIN: Duration = Duration::from_secs(60);

    fn google() -> LaneState {
        LaneState::new(MtLane::builtin(MtService::Google), LimitsTable::PRODUCTION)
    }

    /// Admits and immediately finishes one request with `signal`.
    fn run(state: &mut LaneState, route: MtRoute, now: Instant, signal: MtSignal) {
        assert_eq!(state.decide(route, now, Duration::from_secs(120)), Decision::Admit);
        state.finish(route, now, signal, Duration::ZERO);
    }

    fn blocked() -> MtSignal {
        MtSignal::Blocked { retry_after: None }
    }

    #[test]
    fn spacing_is_measured_from_the_previous_start() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Yandex), LimitsTable::PRODUCTION);
        let t0 = Instant::now();
        assert_eq!(state.decide(MtRoute::Yandex, t0, MIN), Decision::Admit);
        // The request took 0.7 s; the next start is due 1 s after the previous START.
        state.finish(MtRoute::Yandex, t0 + Duration::from_millis(700), MtSignal::Ok, Duration::ZERO);
        assert_eq!(state.decide(MtRoute::Yandex, t0 + Duration::from_millis(700), MIN), Decision::Wait { remaining: Some(Duration::from_millis(300)) });
        assert_eq!(state.decide(MtRoute::Yandex, t0 + Duration::from_secs(1), MIN), Decision::Admit);
    }

    #[test]
    fn in_flight_request_blocks_the_next_one() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Deepl), LimitsTable::PRODUCTION);
        let t0 = Instant::now();
        assert_eq!(state.decide(MtRoute::Deepl, t0, MIN), Decision::Admit);
        assert_eq!(state.decide(MtRoute::Deepl, t0 + Duration::from_secs(10), MIN), Decision::Wait { remaining: None });
        state.finish(MtRoute::Deepl, t0 + Duration::from_secs(10), MtSignal::Neutral, Duration::ZERO);
        assert_eq!(state.decide(MtRoute::Deepl, t0 + Duration::from_secs(10), MIN), Decision::Admit);
    }

    #[test]
    fn burst_of_three_after_idle_then_one_second_spacing() {
        let mut state = google();
        let t0 = Instant::now();
        for _ in 0..3 {
            run(&mut state, MtRoute::GoogleGtx, t0, MtSignal::Ok);
        }
        assert_eq!(state.decide(MtRoute::GoogleGtx, t0, MIN), Decision::Wait { remaining: Some(Duration::from_secs(1)) });
        run(&mut state, MtRoute::GoogleGtx, t0 + Duration::from_secs(1), MtSignal::Ok);
        // 9 s idle is not enough to refill.
        let t1 = t0 + Duration::from_secs(10);
        run(&mut state, MtRoute::GoogleGtx, t1, MtSignal::Ok);
        assert_eq!(state.decide(MtRoute::GoogleGtx, t1, MIN), Decision::Wait { remaining: Some(Duration::from_secs(1)) });
        // 10 s after the last END the burst is back.
        let t2 = t1 + Duration::from_secs(10);
        for _ in 0..3 {
            run(&mut state, MtRoute::GoogleGtx, t2, MtSignal::Ok);
        }
        assert!(matches!(state.decide(MtRoute::GoogleGtx, t2, MIN), Decision::Wait { .. }));
    }

    #[test]
    fn no_burst_refill_while_a_route_cools_or_is_in_probation() {
        let mut state = google();
        let t0 = Instant::now();
        run(&mut state, MtRoute::GoogleGtx, t0, blocked());
        // gtx cools; clients5 still runs but with strict spacing, even after a long idle.
        let t1 = t0 + Duration::from_secs(60);
        run(&mut state, MtRoute::GoogleClients5, t1, MtSignal::Ok);
        assert_eq!(state.decide(MtRoute::GoogleClients5, t1, MIN), Decision::Wait { remaining: Some(Duration::from_secs(1)) });
        // After the cooldown the gtx probe goes out; probation still means strict spacing.
        let t2 = t0 + 5 * MIN + Duration::from_secs(20);
        run(&mut state, MtRoute::GoogleGtx, t2, MtSignal::Ok);
        assert_eq!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Probation { level: 0, successes: 1 }));
        assert_eq!(state.decide(MtRoute::GoogleGtx, t2, MIN), Decision::Wait { remaining: Some(Duration::from_secs(1)) });
        run(&mut state, MtRoute::GoogleGtx, t2 + Duration::from_secs(1), MtSignal::Ok);
        run(&mut state, MtRoute::GoogleGtx, t2 + Duration::from_secs(2), MtSignal::Ok);
        assert!(matches!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Healthy { .. })));
        // Healthy again: the next idle period restores the burst of three.
        let t3 = t2 + Duration::from_secs(20);
        for _ in 0..3 {
            run(&mut state, MtRoute::GoogleGtx, t3, MtSignal::Ok);
        }
    }

    #[test]
    fn full_cooldown_progression_is_5_5_10_20_30_30_minutes() {
        let mut state = google();
        let mut now = Instant::now();
        let mut observed = Vec::new();
        assert_eq!(state.decide(MtRoute::GoogleGtx, now, MIN), Decision::Admit);
        let mut cooldown = state.finish(MtRoute::GoogleGtx, now, blocked(), Duration::ZERO);
        for _ in 0..5 {
            let Some(length) = cooldown else { panic!("a limit signal must cool the route") };
            observed.push(length.as_secs() / 60);
            assert_eq!(state.decide(MtRoute::GoogleGtx, now + length - Duration::from_secs(1), MIN), Decision::CoolingDown { retry_in: Duration::from_secs(1) });
            now += length;
            assert_eq!(state.decide(MtRoute::GoogleGtx, now, MIN), Decision::Admit);
            cooldown = state.finish(MtRoute::GoogleGtx, now, blocked(), Duration::ZERO);
        }
        observed.push(cooldown.map_or(0, |length| length.as_secs() / 60));
        assert_eq!(observed, [5, 5, 10, 20, 30, 30]);
    }

    #[test]
    fn retry_after_is_honoured_and_capped() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Yandex), LimitsTable::PRODUCTION);
        let t0 = Instant::now();
        assert_eq!(state.decide(MtRoute::Yandex, t0, MIN), Decision::Admit);
        // 45 s from the server beats the 30 s first step.
        assert_eq!(state.finish(MtRoute::Yandex, t0, MtSignal::RateLimited { retry_after: Some(Duration::from_secs(45)) }, Duration::ZERO), Some(Duration::from_secs(45)));
        let mut deepl = LaneState::new(MtLane::builtin(MtService::Deepl), LimitsTable::PRODUCTION);
        assert_eq!(deepl.decide(MtRoute::Deepl, t0, MIN), Decision::Admit);
        // DeepL caps Retry-After at 30 min.
        assert_eq!(deepl.finish(MtRoute::Deepl, t0, MtSignal::RateLimited { retry_after: Some(Duration::from_secs(3600)) }, Duration::ZERO), Some(30 * MIN));
    }

    #[test]
    fn probation_failure_steps_up_and_recovery_level_memory_applies() {
        let mut state = google();
        let t0 = Instant::now();
        run(&mut state, MtRoute::GoogleGtx, t0, blocked());
        // Two probe successes, then a limit hit: back to cooling one step further.
        let t1 = t0 + 5 * MIN;
        run(&mut state, MtRoute::GoogleGtx, t1, MtSignal::Ok);
        run(&mut state, MtRoute::GoogleGtx, t1 + Duration::from_secs(1), MtSignal::Ok);
        run(&mut state, MtRoute::GoogleGtx, t1 + Duration::from_secs(2), MtSignal::RateLimited { retry_after: None });
        assert!(matches!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Cooling { level: 1, .. })));
        // Level 1 = 5 min; then three successes restore the route.
        let t2 = t1 + Duration::from_secs(2) + 5 * MIN;
        for offset in 0..3 {
            run(&mut state, MtRoute::GoogleGtx, t2 + Duration::from_secs(offset), MtSignal::Ok);
        }
        let restored = t2 + Duration::from_secs(2);
        assert_eq!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Healthy { restored_at: Some(restored), level: 1 }));
        // A block within 30 min continues the progression (level 2 = 10 min).
        let mut within = state.clone();
        assert_eq!(within.decide(MtRoute::GoogleGtx, restored + 29 * MIN, MIN), Decision::Admit);
        assert_eq!(within.finish(MtRoute::GoogleGtx, restored + 29 * MIN, blocked(), Duration::ZERO), Some(10 * MIN));
        // A block 30 min or more after recovery restarts at the first step.
        assert_eq!(state.decide(MtRoute::GoogleGtx, restored + 30 * MIN, MIN), Decision::Admit);
        assert_eq!(state.finish(MtRoute::GoogleGtx, restored + 30 * MIN, blocked(), Duration::ZERO), Some(5 * MIN));
    }

    #[test]
    fn deepl_cooldowns_are_refused_at_once_and_escalate_within_the_level_memory() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Deepl), LimitsTable::PRODUCTION);
        let mut now = Instant::now();
        let mut minutes = Vec::new();
        for _ in 0..5 {
            assert_eq!(state.decide(MtRoute::Deepl, now, MIN), Decision::Admit);
            let Some(length) = state.finish(MtRoute::Deepl, now, MtSignal::RateLimited { retry_after: None }, Duration::ZERO) else { panic!("rate limit must cool DeepL") };
            minutes.push(length.as_secs() / 60);
            // Never waited out in place, whatever the caller's budget.
            assert_eq!(state.decide(MtRoute::Deepl, now, 60 * MIN), Decision::CoolingDown { retry_in: length });
            now += length;
        }
        assert_eq!(minutes, [2, 5, 10, 15, 15]);
        // A success ends the probation; a limit 29 min later continues at the last step.
        run(&mut state, MtRoute::Deepl, now, MtSignal::Ok);
        let mut within = state.clone();
        let later = now + 29 * MIN;
        assert_eq!(within.decide(MtRoute::Deepl, later, MIN), Decision::Admit);
        assert_eq!(within.finish(MtRoute::Deepl, later, MtSignal::RateLimited { retry_after: None }, Duration::ZERO), Some(15 * MIN));
        // 30 min or more after recovery the progression restarts at 2 min.
        let much_later = now + 30 * MIN;
        assert_eq!(state.decide(MtRoute::Deepl, much_later, MIN), Decision::Admit);
        assert_eq!(state.finish(MtRoute::Deepl, much_later, MtSignal::RateLimited { retry_after: None }, Duration::ZERO), Some(2 * MIN));
    }

    fn deepl() -> LaneState {
        LaneState::new(MtLane::builtin(MtService::Deepl), LimitsTable::PRODUCTION)
    }

    /// Admits a request at `now` and ends it `Ok` at once.
    fn admit_ok(state: &mut LaneState, now: Instant) {
        assert_eq!(state.decide(MtRoute::Deepl, now, 2 * MIN), Decision::Admit);
        state.finish(MtRoute::Deepl, now, MtSignal::Ok, Duration::ZERO);
    }

    #[test]
    fn a_fresh_bucket_grants_six_back_to_back_then_one_per_minute() {
        let mut state = deepl();
        let t0 = Instant::now();
        for _ in 0..6 {
            admit_ok(&mut state, t0);
        }
        assert_eq!(state.decide(MtRoute::Deepl, t0, 2 * MIN), Decision::Wait { remaining: Some(MIN) });
        assert_eq!(state.decide(MtRoute::Deepl, t0 + Duration::from_secs(59), 2 * MIN), Decision::Wait { remaining: Some(Duration::from_secs(1)) });
        admit_ok(&mut state, t0 + MIN);
        assert_eq!(state.decide(MtRoute::Deepl, t0 + MIN, 2 * MIN), Decision::Wait { remaining: Some(MIN) });
    }

    #[test]
    fn partial_refill_keeps_the_fraction_of_the_next_token() {
        let mut state = deepl();
        let t0 = Instant::now();
        for _ in 0..6 {
            admit_ok(&mut state, t0);
        }
        // 150 s later: two whole tokens, the third is 30 s into its minute.
        let t1 = t0 + Duration::from_secs(150);
        admit_ok(&mut state, t1);
        admit_ok(&mut state, t1);
        assert_eq!(state.decide(MtRoute::Deepl, t1, 2 * MIN), Decision::Wait { remaining: Some(Duration::from_secs(30)) });
        admit_ok(&mut state, t1 + Duration::from_secs(30));
    }

    #[test]
    fn idleness_refills_up_to_the_capacity_only() {
        let mut state = deepl();
        let t0 = Instant::now();
        for _ in 0..6 {
            admit_ok(&mut state, t0);
        }
        // An hour idle refills six tokens, not sixty.
        let t1 = t0 + 60 * MIN;
        for _ in 0..6 {
            admit_ok(&mut state, t1);
        }
        assert_eq!(state.decide(MtRoute::Deepl, t1, 2 * MIN), Decision::Wait { remaining: Some(MIN) });
    }

    #[test]
    fn a_token_wait_beyond_the_caller_budget_is_refused() {
        let mut state = deepl();
        let t0 = Instant::now();
        for _ in 0..6 {
            admit_ok(&mut state, t0);
        }
        assert_eq!(state.decide(MtRoute::Deepl, t0, Duration::from_secs(30)), Decision::CoolingDown { retry_in: MIN });
        // Nothing was admitted: the full minute is still owed.
        assert_eq!(state.decide(MtRoute::Deepl, t0, MIN), Decision::Wait { remaining: Some(MIN) });
    }

    #[test]
    fn a_limit_signal_empties_the_bucket() {
        let mut state = deepl();
        let t0 = Instant::now();
        admit_ok(&mut state, t0);
        assert_eq!(state.decide(MtRoute::Deepl, t0, MIN), Decision::Admit);
        state.finish(MtRoute::Deepl, t0, MtSignal::RateLimited { retry_after: None }, Duration::ZERO);
        // After the 2 min cooldown only the two tokens accrued since the 429 are left.
        let t1 = t0 + 2 * MIN;
        admit_ok(&mut state, t1);
        admit_ok(&mut state, t1);
        assert_eq!(state.decide(MtRoute::Deepl, t1, 2 * MIN), Decision::Wait { remaining: Some(MIN) });
    }

    #[test]
    fn services_without_a_budget_only_space() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Yandex), LimitsTable::PRODUCTION);
        let t0 = Instant::now();
        for second in 0..30 {
            run(&mut state, MtRoute::Yandex, t0 + Duration::from_secs(second), MtSignal::Ok);
        }
    }

    #[test]
    fn caller_budget_bounds_the_cooldown_wait() {
        let mut state = LaneState::new(MtLane::builtin(MtService::Yandex), LimitsTable::PRODUCTION);
        let t0 = Instant::now();
        run(&mut state, MtRoute::Yandex, t0, MtSignal::RateLimited { retry_after: None });
        assert_eq!(state.decide(MtRoute::Yandex, t0, MIN), Decision::Wait { remaining: Some(Duration::from_secs(30)) });
        assert_eq!(state.decide(MtRoute::Yandex, t0, Duration::from_secs(10)), Decision::CoolingDown { retry_in: Duration::from_secs(30) });
    }

    #[test]
    fn neutral_signals_never_change_health() {
        let mut state = google();
        let t0 = Instant::now();
        run(&mut state, MtRoute::GoogleGtx, t0, MtSignal::Neutral);
        assert_eq!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Healthy { restored_at: None, level: 0 }));
        run(&mut state, MtRoute::GoogleGtx, t0, blocked());
        let t1 = t0 + 5 * MIN;
        run(&mut state, MtRoute::GoogleGtx, t1, MtSignal::Neutral);
        assert_eq!(state.route_health(MtRoute::GoogleGtx), Some(RouteHealth::Probation { level: 0, successes: 0 }));
    }

    #[test]
    fn service_cooldown_only_when_every_route_cools() {
        let mut state = google();
        let t0 = Instant::now();
        run(&mut state, MtRoute::GoogleGtx, t0, blocked());
        assert_eq!(state.lane_cooldown(t0), None);
        let t1 = t0 + MIN;
        assert_eq!(state.decide(MtRoute::GoogleClients5, t1, MIN), Decision::Admit);
        state.finish(MtRoute::GoogleClients5, t1, MtSignal::RateLimited { retry_after: None }, Duration::ZERO);
        // gtx has 4 min left, clients5 5 min: the service is back when the first route is.
        assert_eq!(state.lane_cooldown(t1), Some(4 * MIN));
        assert_eq!(state.lane_cooldown(t0 + 5 * MIN), None);
    }
}
