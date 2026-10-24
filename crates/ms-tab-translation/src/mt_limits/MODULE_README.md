# Module: crates/ms-tab-translation/src/mt_limits

## Purpose
The one owner of machine-translation request pacing and cooldowns for the providers
of Google, DeepL and Yandex, over built-in HTTP and over the browser extension (separate lanes).
It decides when a provider may send a request, waits (cancellably, reported) when it must, and
refuses at once when a lane is throttled for longer than the
caller may wait. It also owns the split of texts that exceed a request size limit. GUI-free:
no egui, no i18n (callers localize `MtAcquireError` themselves).

## Architecture
```text
provider (machine_translators/*)          caller (run loop / translate_texts_via_translator)
   |  call.acquire(route) -> MtPermit         builds MtCall::for_run(...) / MtCall::detached()
   |  <one HTTP request>
   |  permit.finish(MtSignal)
   v
MtCall (call.rs)  -- sliced, cancellable wait loop; reports MtWaitStatus
   v
MtLimiterRegistry::shared() (registry.rs)  -- Mutex<HashMap<MtLane, LaneState>> + MtClock
   v
LaneState (limiter.rs)  -- pure decide/finish over numbers from table.rs
```
- `table.rs` holds every number (units in names) and the route list; `limiter.rs` applies the
  rules (header of `limiter.rs`); `registry.rs` adds the lock and the clock; `call.rs` adds the
  wait loop, cancel, reporting and the permit; `split.rs` is independent text logic.
- One process-wide registry: a Translation run (also a detached one still finishing after
  Stop) and the Cleaning tab's prompt translator share it, so concurrent callers can never
  exceed a lane's rate. This static is a documented exception to global §5 (header of
  `registry.rs`).
- **Lanes.** Pacing state is kept per `MtLane { service, transport }`
  (`ms_config::mt_transport::MtTransport`: `Builtin` HTTP or the user's `Browser` extension):
  the two transports of one service are separate lanes with their own numbers, slot, budget and
  routes, so neither ever spends or blocks the other's quota. Every route belongs to one lane
  (`MtRoute::lane`). The persisted per-service choice is `ms_config::mt_transport`.
- Pacing per lane has up to three layers, all checked in one decision: one request in flight
  (`max_in_flight`), start-to-start spacing with a burst after idleness (`min_interval`,
  `burst`, `burst_idle`), and an optional request budget (`RequestBudget`, a token bucket:
  `capacity` requests back to back from a full bucket, one more per `refill_every`) for a
  lane whose server counts requests per client instead of spacing them. Cooldowns sit on
  top, per route.

## Files and submodules
- `table.rs`: `MtLane`, `MtRoute`, `LaneLimits` / `lane_limits`, `RequestBudget`, `RouteLimits` / `route_limits`,
  `routes_of`, `LimitsTable` (production or a test table). Edit to change any number.
- `limiter.rs`: `LaneState`, `TokenBucket`, `RouteHealth`, `Decision`; the pacing / burst /
  budget / cooldown / probation rules. Pure: `now` and jitter come in as arguments.
- `registry.rs`: `MtClock`, `SystemClock`, `MtLimiterRegistry` (`shared`, `with_table`,
  test-only `new`), the poisoned-lock recovery and the cooldown log line.
- `call.rs`: `MtCall` (`for_run`, `detached`, `acquire`, `is_cancelled`, `lane_cooldown`,
  `report_attention`; test-only `with_max_wait`), `MtPermit`, `MtSignal`, `MtAcquireError`,
  `MtWaitStatus` + `MtWaitReason`,
  `RUN_MAX_WAIT` (120 s), `DETACHED_MAX_WAIT` (30 s).
- `split.rs`: `split_for_request` (chars), `split_for_request_in` + `SizeUnit` (chars or
  UTF-16 code units, the browser lanes' unit), `RequestChunk`, `join_chunks`.
- `test_support.rs` (tests only): `FakeClock` — sleep advances time, asserts the registry lock
  is free; for this module's and the providers' tests.

## Routes and consumers
- Google: `GoogleGtx` (both gtx hosts) and `GoogleClients5`; 1 s spacing, burst 3 after 10 s
  idle, cooldowns 5/5/10/20/30 min, never waited out (`max_cooldown_wait` 0: the provider
  skips to its next route), 3 probe successes to recover, 5000-char request limit (split).
- DeepL: `Deepl`; no spacing, request budget 6 with one token per 60 s (measured 2026-10-23
  from one IP: ~6-7 requests from idle, then ~1 per 55-60 s, whatever the spacing), cooldowns
  2/5/10/15 min without jitter, never waited out (`max_cooldown_wait` 0: the run stops with
  `CoolingDown`), level memory 30 min, `Retry-After` honoured up to 30 min, no provider
  re-sends (requests sent while limited escalated DeepL's block past 20 min).
- Yandex: `Yandex`; 1 s spacing, cooldowns 30 s / 60 s / 2 min / 5 min, waited out up to 60 s.
- Browser lanes (the user's browser through the extension): `GoogleBrowser`, `DeeplBrowser`,
  `YandexBrowser`, one route per lane. GUESSED numbers, not measured: one in flight, 2 s spacing,
  burst 1, no request budget, request limit 1400 (DeepL) / 4500 (Google, Yandex) UTF-16 code
  units, strictly below the extension's site limits 1500 / 5000 (mirror test in
  `machine_translators/browser.rs`), no
  provider re-sends, cooldowns 1/2/5/10 min never waited out in place, 1 success to recover,
  level memory 30 min.
- Callers: `machine_translation.rs` (`MtCall::for_run` per Translation run, `MtCall::detached`
  in `translate_texts_via_translator`); per-provider use is described in
  `../machine_translators/MODULE_README.md`.

## Contracts and invariants
- A provider takes a permit before EVERY HTTP request and finishes it with the request's
  `MtSignal`; it never sleeps, retries on a timer or throttles by itself. A dropped,
  unfinished permit counts as `Neutral` and always frees the slot.
- Signals: `Ok`; `RateLimited` / `Blocked` (cool the route, `Retry-After` honoured up to the
  service cap); `Neutral` (network, other statuses, format) never changes health.
- At most `max_in_flight` (1 for every lane) requests of a lane are in flight
  process-wide. Spacing is start-to-start; the burst refills only after `burst_idle` of
  idleness and never while a route of the lane cools or is in probation.
- Request budget: admission takes a token; an empty bucket is a `Wait` with the exact time to
  the next token (reported like any long wait), or `CoolingDown` with that time when it exceeds
  the caller's remaining budget (so a Translation run, 120 s, always waits a token out; the
  detached caller, 30 s, may be refused). Idleness refills only up to `capacity`. A
  `RateLimited` / `Blocked` signal empties the bucket (the server says the budget is spent).
  Tokens are integers plus the instant the next one accrues from, so a fake clock drives the
  bucket exactly.
- Any limit hit, wherever it comes from, moves only the route it happened on; a lane is
  "cooling" (`lane_cooldown`) only when all of its routes are.
- `acquire` never sends anything after `Cancelled` / `CoolingDown` / `Busy`; it returns
  `CoolingDown` at once (no sleep) when the remaining cooldown exceeds
  `min(route max_cooldown_wait, caller budget)`. Waits sleep in slices of at most 250 ms
  through the registry's clock, so a cancel is seen within one slice.
- The registry lock is held only for one pure decision: never across a sleep, the wait
  reporter callback, a log line or I/O. A poisoned lock is recovered and logged.
- Wait reports: `Some(MtWaitStatus)` only for waits expected to reach 1.5 s AND to exceed the
  lane's ordinary spacing (`min_interval`; so the normal pace stays silent, while a
  cooldown, a token wait or queueing behind another request is shown), once per change of the shown whole
  second; then exactly one `None` when the wait ends (any outcome). Every limiter report carries
  `MtWaitReason::Limiter`. Shown seconds are rounded up by `call::whole_seconds_ceil`, the one
  owner of that rule.
- Attention reports: a browser-extension provider calls `MtCall::report_attention(service,
  active)` while it holds a permit; a change to `true` reports `Some` with
  `MtWaitReason::BrowserAttention` and `remaining: None`, a change to `false` reports `None`,
  repeats are silent, and dropping the call ends a notice still shown. They never overlap an
  `acquire` wait of the same call.
- Waiters are not FIFO. Cooldown state is in memory only; a restart forgets it.
- Split: `text + separator` of every chunk concatenated reproduces the input exactly; chunk
  lengths are counted in `char`s.

## Editing map
- Change a limit, a request budget, a cooldown step or a route: `table.rs` (and its tests).
- Change how a signal moves a route between healthy / cooling / probation: `limiter.rs`.
- Change waiting, cancel or reporting behaviour: `call.rs`.
- Add a service or a transport: a `MtService` / `MtTransport` variant forces the exhaustive
  matches in `table.rs`; add its lane numbers and routes there. Provider wiring lives in
  `../machine_translators/`.
- Change how long texts are cut: `split.rs`.
