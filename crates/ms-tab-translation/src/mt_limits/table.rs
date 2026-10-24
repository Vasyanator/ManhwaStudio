/*
File: crates/ms-tab-translation/src/mt_limits/table.rs

Purpose:
The ONE place that holds the request-rate numbers of the machine-translation services: how
often a service may be asked, how many requests may overlap, how long a throttled route cools
down and how it recovers. Everything else in `mt_limits` reads these tables; no provider
spells a number of its own.

Key items:
- `MtLane`: a service reached through one transport (`ms_config::mt_transport::MtTransport`);
  the unit that owns pacing state. Built-in HTTP and the browser extension are separate lanes
  of the same service, so one never spends the other's quota.
- `MtRoute`: the independently throttled endpoint groups (built-in: Google's two gtx hosts
  share one route, `clients5` is another, DeepL and Yandex have one each; browser: one route
  per service). Every route belongs to one lane (`MtRoute::lane`).
- `LaneLimits` / `lane_limits()`: per-lane pacing (in-flight cap, start-to-start
  spacing, burst after idle, optional request budget, request size, provider retries,
  `Retry-After` cap).
- `RequestBudget`: a token bucket (capacity + one token per refill period) for a service whose
  server counts requests per client rather than spacing them.
- `RouteLimits` / `route_limits()`: per-route cooldown progression and recovery.
- `routes_of()`: the routes a lane owns (a lane is "cooling" only when all of them are).
- `LimitsTable`: the pair of table functions a registry runs on (production or a test table).

Notes:
The numbers are conservative: Google's are derived from a measured block (gtx hosts blocked
together for ~28 min while `clients5` answered); DeepL's request budget and minute-scale
cooldowns come from a measurement of its web JSON-RPC endpoint (2026-10-23, one IP: ~6-7
requests from an idle client, then ~1 request per 55-60 s whatever the spacing; requests sent
while limited escalate the block from minutes to more than 20 min); Yandex's are guesses
(nothing is published for the endpoint). The browser lanes' numbers are GUESSES, not
measurements: the user's browser talks to the services' own web pages, whose limits nobody
has measured from the extension; they keep one request in flight, 2 s apart, and stop the run
on the first limit instead of waiting minutes out in place. Their `max_request_chars` is in
UTF-16 code units (the extension checks JavaScript's `String.length`) and stays strictly below
the extension's own per-site `maxChars` (`browser-extension/src/modules/translate/sites/<site>.ts`;
a mirror test in `machine_translators::browser` pins the margin).
*/

use std::time::Duration;

use ms_config::mt_transport::MtTransport;

use crate::machine_translation::MtService;

/// One minute, for readable cooldown steps.
const MINUTE: Duration = Duration::from_secs(60);

/// Google's gtx cooldown progression (both routes): 5, 5, 10, 20, 30 min, then 30 min forever.
const GOOGLE_COOLDOWN_STEPS: &[Duration] = &[
    Duration::from_secs(5 * 60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

/// DeepL's cooldown progression: 2, 5, 10, 15 min, then 15 min forever. Minutes, not seconds:
/// the measured un-escalated block cleared within 2 min, and every request sent while blocked
/// prolonged it (see the file header).
const DEEPL_COOLDOWN_STEPS: &[Duration] = &[Duration::from_secs(2 * 60), Duration::from_secs(5 * 60), Duration::from_secs(10 * 60), Duration::from_secs(15 * 60)];

/// Every browser lane's cooldown progression: 1, 2, 5, 10 min, then 10 min forever (a guess,
/// see the file header).
const BROWSER_COOLDOWN_STEPS: &[Duration] = &[MINUTE, Duration::from_secs(2 * 60), Duration::from_secs(5 * 60), Duration::from_secs(10 * 60)];

/// Yandex's cooldown progression: 30 s, 60 s, 2 min, 5 min.
const YANDEX_COOLDOWN_STEPS: &[Duration] = &[Duration::from_secs(30), Duration::from_secs(60), Duration::from_secs(2 * 60), Duration::from_secs(5 * 60)];

/// One limiter lane: a machine-translation service reached through one transport. Pacing state
/// (in-flight slot, spacing, burst, request budget) is kept per lane, so the built-in HTTP
/// requests and the browser extension never throttle each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct MtLane {
    /// The service the lane talks to.
    pub(crate) service: MtService,
    /// How the service is reached.
    pub(crate) transport: MtTransport,
}

impl MtLane {
    /// The built-in HTTP lane of `service`.
    #[must_use]
    pub(crate) const fn builtin(service: MtService) -> Self {
        Self { service, transport: MtTransport::Builtin }
    }

    /// The browser-extension lane of `service`.
    #[must_use]
    pub(crate) const fn browser(service: MtService) -> Self {
        Self { service, transport: MtTransport::Browser }
    }
}

/// An independently throttled endpoint group of one machine-translation lane.
///
/// A route cools down on its own; a lane is refused as a whole only when every route of
/// it is cooling (`routes_of`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MtRoute {
    /// Google `translate_a/single` with client `gtx`, on BOTH `translate.googleapis.com` and
    /// `translate.google.com`: measured to be blocked together.
    GoogleGtx,
    /// Google `clients5.google.com/translate_a/t` (client `dict-chrome-ex`), measured to stay
    /// open while the gtx hosts were blocked.
    GoogleClients5,
    /// The DeepL JSON-RPC web endpoint.
    Deepl,
    /// The Yandex translate endpoint.
    Yandex,
    /// Google Translate driven in the user's browser through the extension.
    GoogleBrowser,
    /// DeepL driven in the user's browser through the extension.
    DeeplBrowser,
    /// Yandex Translate driven in the user's browser through the extension.
    YandexBrowser,
}

impl MtRoute {
    /// The lane this route belongs to (its pacing state and its sibling routes).
    #[must_use]
    pub(crate) const fn lane(self) -> MtLane {
        match self {
            MtRoute::GoogleGtx | MtRoute::GoogleClients5 => MtLane::builtin(MtService::Google),
            MtRoute::Deepl => MtLane::builtin(MtService::Deepl),
            MtRoute::Yandex => MtLane::builtin(MtService::Yandex),
            MtRoute::GoogleBrowser => MtLane::browser(MtService::Google),
            MtRoute::DeeplBrowser => MtLane::browser(MtService::Deepl),
            MtRoute::YandexBrowser => MtLane::browser(MtService::Yandex),
        }
    }

    /// Stable lower-case name for logs.
    #[must_use]
    pub(crate) const fn log_name(self) -> &'static str {
        match self {
            MtRoute::GoogleGtx => "google-gtx",
            MtRoute::GoogleClients5 => "google-clients5",
            MtRoute::Deepl => "deepl",
            MtRoute::Yandex => "yandex",
            MtRoute::GoogleBrowser => "google-browser",
            MtRoute::DeeplBrowser => "deepl-browser",
            MtRoute::YandexBrowser => "yandex-browser",
        }
    }
}

/// Pacing of one lane, shared by all of its routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LaneLimits {
    /// Most requests of the lane in flight at once (process-wide).
    pub(crate) max_in_flight: u32,
    /// Minimum time from one request start to the next start.
    pub(crate) min_interval: Duration,
    /// Requests that may start without `min_interval` spacing after the lane was idle.
    pub(crate) burst: u32,
    /// Idle time (since the last request ended) after which the burst is granted again. For a
    /// lane with `burst == 1` it equals `min_interval`, so the burst never skips spacing.
    pub(crate) burst_idle: Duration,
    /// Token bucket every request of the lane draws from, on top of the spacing; `None` =
    /// the spacing and burst alone pace the lane.
    pub(crate) request_budget: Option<RequestBudget>,
    /// Longest text one request may carry; longer texts are split (`split::split_for_request`).
    /// Measured in `char`s on the built-in lanes and in UTF-16 code units on the browser lanes
    /// (`split::SizeUnit`). `None` = no limit.
    pub(crate) max_request_chars: Option<usize>,
    /// How many times a provider re-sends a request after a rate-limit answer before it gives
    /// the item up (each re-send waits in the next `acquire`).
    pub(crate) rate_limit_retries: u32,
    /// Upper bound applied to a server's `Retry-After` before it lengthens a cooldown.
    pub(crate) retry_after_cap: Duration,
}

/// A per-lane request budget (token bucket): `capacity` requests may start back to back from
/// a full bucket, and one more is granted every `refill_every`, never above `capacity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestBudget {
    /// Tokens of a full bucket (at least 1).
    pub(crate) capacity: u32,
    /// Time that grants one token (non-zero).
    pub(crate) refill_every: Duration,
}

/// Cooldown and recovery rules of one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RouteLimits {
    /// Cooldown per consecutive limit hit; the last step repeats. Never empty.
    pub(crate) cooldown_steps: &'static [Duration],
    /// Half-open random jitter range `[lo, hi)` added to a cooldown step; `(0, 0)` = none.
    pub(crate) jitter: (Duration, Duration),
    /// Longest remaining cooldown a request waits out in place; a longer one is refused at
    /// once (`CoolingDown`) so the caller can fall back or stop. Zero = never wait.
    pub(crate) max_cooldown_wait: Duration,
    /// Successful probes needed after a cooldown before the route counts as healthy again.
    pub(crate) recovery_successes: u32,
    /// How long after recovery a new limit hit continues the cooldown progression instead of
    /// restarting it at the first step. Zero = every recovery resets the progression.
    pub(crate) level_memory: Duration,
}

/// The production pacing of `lane`.
#[must_use]
pub(crate) const fn lane_limits(lane: MtLane) -> LaneLimits {
    match lane.transport {
        MtTransport::Builtin => builtin_limits(lane.service),
        MtTransport::Browser => browser_limits(lane.service),
    }
}

/// The production pacing of `service`'s built-in HTTP lane.
const fn builtin_limits(service: MtService) -> LaneLimits {
    match service {
        MtService::Google => LaneLimits {
            max_in_flight: 1,
            min_interval: Duration::from_secs(1),
            burst: 3,
            burst_idle: Duration::from_secs(10),
            request_budget: None,
            max_request_chars: Some(5000),
            // The endpoint chain is the fallback; a provider never re-sends to the same route.
            rate_limit_retries: 0,
            retry_after_cap: Duration::from_secs(30 * 60),
        },
        MtService::Deepl => LaneLimits {
            max_in_flight: 1,
            // No spacing: DeepL counts requests, it does not space them (the same count was
            // rejected at 10 s and 20 s spacing), and a batch's split -> handle_jobs pair was
            // accepted back to back. The request budget below is the real limit.
            min_interval: Duration::ZERO,
            burst: 1,
            burst_idle: Duration::ZERO,
            // Measured 2026-10-23 from one IP: ~6-7 requests from an idle client, then about one
            // per 55-60 s. 6 per 60 s stays a little under both.
            request_budget: Some(RequestBudget { capacity: 6, refill_every: MINUTE }),
            max_request_chars: None,
            // Never re-send after a rate limit: every request sent while limited escalated the
            // block (code 1042901, cleared < 2 min, became 1042912, still blocked > 20 min later).
            rate_limit_retries: 0,
            retry_after_cap: Duration::from_secs(30 * 60),
        },
        MtService::Yandex => LaneLimits {
            max_in_flight: 1,
            min_interval: Duration::from_secs(1),
            burst: 1,
            burst_idle: Duration::from_secs(1),
            request_budget: None,
            max_request_chars: None,
            rate_limit_retries: 0,
            retry_after_cap: Duration::from_secs(10 * 60),
        },
    }
}

/// The production pacing of `service`'s browser-extension lane (guesses, see the file header).
const fn browser_limits(service: MtService) -> LaneLimits {
    // UTF-16 code units, below the extension's site limits (DeepL 1500, Google / Yandex 5000)
    // with a margin, so a request the app packs is never refused there as `too_long`.
    let max_request_chars = match service {
        MtService::Deepl => 1400,
        MtService::Google | MtService::Yandex => 4500,
    };
    LaneLimits {
        max_in_flight: 1,
        min_interval: Duration::from_secs(2),
        burst: 1,
        burst_idle: Duration::from_secs(2),
        request_budget: None,
        max_request_chars: Some(max_request_chars),
        rate_limit_retries: 0,
        retry_after_cap: Duration::from_secs(10 * 60),
    }
}

/// The production cooldown rules of `route`.
#[must_use]
pub(crate) const fn route_limits(route: MtRoute) -> RouteLimits {
    match route {
        MtRoute::GoogleGtx | MtRoute::GoogleClients5 => RouteLimits {
            cooldown_steps: GOOGLE_COOLDOWN_STEPS,
            jitter: (Duration::ZERO, Duration::ZERO),
            // Never wait: the provider skips to the next route instead.
            max_cooldown_wait: Duration::ZERO,
            // The measured block is leaky (single requests pass), so one 200 proves nothing.
            recovery_successes: 3,
            level_memory: Duration::from_secs(30 * 60),
        },
        MtRoute::Deepl => RouteLimits {
            cooldown_steps: DEEPL_COOLDOWN_STEPS,
            // No re-sends to de-synchronise, so no jitter.
            jitter: (Duration::ZERO, Duration::ZERO),
            // Never waited out in place: the run stops (`CoolingDown`, "try again in N min")
            // instead of sitting minutes in a wait the user cannot see the end of.
            max_cooldown_wait: Duration::ZERO,
            recovery_successes: 1,
            // A repeat limit within 30 min of recovering continues the progression: the measured
            // escalated block outlasted 20 min.
            level_memory: Duration::from_secs(30 * 60),
        },
        MtRoute::Yandex => RouteLimits {
            cooldown_steps: YANDEX_COOLDOWN_STEPS,
            jitter: (Duration::ZERO, Duration::ZERO),
            max_cooldown_wait: MINUTE,
            recovery_successes: 1,
            level_memory: Duration::from_secs(10 * 60),
        },
        MtRoute::GoogleBrowser | MtRoute::DeeplBrowser | MtRoute::YandexBrowser => RouteLimits {
            cooldown_steps: BROWSER_COOLDOWN_STEPS,
            jitter: (Duration::ZERO, Duration::ZERO),
            // Never waited out in place: the run stops (`CoolingDown`) and the user retries.
            max_cooldown_wait: Duration::ZERO,
            recovery_successes: 1,
            level_memory: Duration::from_secs(30 * 60),
        },
    }
}

/// Every route of `lane`, in the provider's preferred order.
#[must_use]
pub(crate) const fn routes_of(lane: MtLane) -> &'static [MtRoute] {
    match (lane.service, lane.transport) {
        (MtService::Google, MtTransport::Builtin) => &[MtRoute::GoogleGtx, MtRoute::GoogleClients5],
        (MtService::Deepl, MtTransport::Builtin) => &[MtRoute::Deepl],
        (MtService::Yandex, MtTransport::Builtin) => &[MtRoute::Yandex],
        (MtService::Google, MtTransport::Browser) => &[MtRoute::GoogleBrowser],
        (MtService::Deepl, MtTransport::Browser) => &[MtRoute::DeeplBrowser],
        (MtService::Yandex, MtTransport::Browser) => &[MtRoute::YandexBrowser],
    }
}

/// The table a registry runs on: production (`LimitsTable::PRODUCTION`) or a test table with
/// short intervals. Plain function pointers keep the table `Copy` and allocation-free.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LimitsTable {
    /// Pacing of a lane.
    pub(crate) lane: fn(MtLane) -> LaneLimits,
    /// Cooldown rules of a route.
    pub(crate) route: fn(MtRoute) -> RouteLimits,
}

impl LimitsTable {
    /// The production numbers above.
    pub(crate) const PRODUCTION: Self = Self { lane: lane_limits, route: route_limits };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_belongs_to_its_service_and_has_steps() {
        for &service in MtService::all() {
            for &route in routes_of(MtLane::builtin(service)) {
                assert_eq!(route.lane(), MtLane::builtin(service));
                assert!(!route_limits(route).cooldown_steps.is_empty(), "{}", route.log_name());
            }
            let limits = lane_limits(MtLane::builtin(service));
            assert!(limits.max_in_flight >= 1);
            assert!(limits.burst >= 1);
            if let Some(budget) = limits.request_budget {
                assert!(budget.capacity >= 1 && !budget.refill_every.is_zero(), "{service:?}");
            }
            // A burst of one must never skip spacing (see `LaneLimits::burst_idle`).
            if limits.burst == 1 {
                assert!(limits.burst_idle >= limits.min_interval);
            }
        }
    }

    #[test]
    fn google_progression_matches_the_decision() {
        let minutes: Vec<u64> = route_limits(MtRoute::GoogleGtx).cooldown_steps.iter().map(|d| d.as_secs() / 60).collect();
        assert_eq!(minutes, [5, 5, 10, 20, 30]);
        assert_eq!(lane_limits(MtLane::builtin(MtService::Google)).max_request_chars, Some(5000));
    }

    #[test]
    fn deepl_numbers_match_the_measurement() {
        let limits = lane_limits(MtLane::builtin(MtService::Deepl));
        assert_eq!(limits.request_budget, Some(RequestBudget { capacity: 6, refill_every: MINUTE }));
        assert_eq!(limits.rate_limit_retries, 0);
        let route = route_limits(MtRoute::Deepl);
        let minutes: Vec<u64> = route.cooldown_steps.iter().map(|d| d.as_secs() / 60).collect();
        assert_eq!(minutes, [2, 5, 10, 15]);
        // Below the first step: a DeepL cooldown is never waited out in place.
        assert!(route.cooldown_steps.first().is_some_and(|first| route.max_cooldown_wait < *first));
        assert!(route.level_memory >= Duration::from_secs(30 * 60));
        assert_eq!(lane_limits(MtLane::builtin(MtService::Google)).request_budget, None);
        assert_eq!(lane_limits(MtLane::builtin(MtService::Yandex)).request_budget, None);
    }

    #[test]
    fn every_browser_route_is_on_its_own_lane_with_the_decided_numbers() {
        for &service in MtService::all() {
            let lane = MtLane::browser(service);
            let routes = routes_of(lane);
            assert_eq!(routes.len(), 1, "{service:?}");
            for &route in routes {
                assert_eq!(route.lane(), lane);
                assert!(!routes_of(MtLane::builtin(service)).contains(&route));
                let limits = route_limits(route);
                let minutes: Vec<u64> = limits.cooldown_steps.iter().map(|d| d.as_secs() / 60).collect();
                assert_eq!(minutes, [1, 2, 5, 10]);
                assert_eq!(limits.max_cooldown_wait, Duration::ZERO);
                assert_eq!(limits.recovery_successes, 1);
                assert_eq!(limits.level_memory, Duration::from_secs(30 * 60));
            }
            let limits = lane_limits(lane);
            assert_eq!(limits.max_in_flight, 1);
            assert_eq!((limits.min_interval, limits.burst), (Duration::from_secs(2), 1));
            assert!(limits.burst_idle >= limits.min_interval);
            assert_eq!(limits.request_budget, None);
            assert_eq!(limits.rate_limit_retries, 0);
        }
        assert_eq!(lane_limits(MtLane::browser(MtService::Deepl)).max_request_chars, Some(1400));
        assert_eq!(lane_limits(MtLane::browser(MtService::Google)).max_request_chars, Some(4500));
        assert_eq!(lane_limits(MtLane::browser(MtService::Yandex)).max_request_chars, Some(4500));
    }
}
