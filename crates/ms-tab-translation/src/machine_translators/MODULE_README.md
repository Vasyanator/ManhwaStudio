# Module: crates/ms-tab-translation/src/machine_translators

## Purpose
Machine-translation provider implementations (Google, DeepL, Yandex) used by the Translation
tab's plain-MT run and by the Cleaning tab's prompt translator, over two transports: the
program's own HTTP requests (`google.rs`, `deepl.rs`, `yandex.rs`) and the user's browser
through the ManhwaStudio extension (`browser.rs`).

## Architecture
`mod.rs` defines the `MachineTranslatorBackend` trait: a synchronous batch contract that takes
the caller's limiter handle (`crate::mt_limits::call::MtCall`) and returns one
`Result<String, MtItemError>` per input text, plus `preferred_batch_size` (default 1). Two ways
in:
- A Translation run builds ONE backend per run through `run_session(service, transport,
  cancel)` (built-in: each provider's own `run_session()`; browser: `browser::run_session`, which
  also takes the run's Stop flag for the bridge), so a provider keeps run-scoped state (pooled
  agent, endpoint memory, the connection check), and calls it with `MtCall::for_run`
  (cancellable, reported waits). Process-wide provider state (DeepL's HTTP agent, Yandex UCID)
  outlives runs on purpose.
- `machine_translation::translate_texts_via_translator` (the Cleaning tab's entry) reads the live
  transport (`crate::mt_transport`) and calls the stateless `*MtBackend` types, or a fresh
  `browser::detached_session`, with `MtCall::detached()`; each builds a fresh session per call.

Pacing and cooldowns are NOT decided here: every provider takes a permit of its route from
`MtCall::acquire` before EVERY HTTP request and finishes it with the request's `MtSignal`
(`../mt_limits/MODULE_README.md` owns the rules and the numbers). No provider sleeps, retries on
a timer or throttles itself.

This directory is UI-agnostic: no `egui`, `CanvasView`, `ProjectData` or panels. Providers use
blocking HTTP because they run only on MT worker threads. The native HTTP stack is not compiled
for wasm; there every provider is a stub that fails with a clear "unavailable on web" error.

## Files and submodules
- `mod.rs`: provider modules, `MachineTranslatorBackend`, `MtItemError` / `MtItemErrorKind`
  and the `run_session` factory.
- `http.rs` (native): the one owner of the providers' HTTP transport facts — timeouts (10 s
  connect, 30 s per read, 40 s overall; `build_agent`), redirect choice, and `Retry-After`
  parsing (`retry_after`: delta-seconds only, an HTTP-date is ignored). No provider builds an
  agent or picks a timeout of its own.
- `google.rs`: Google Translate over Google's free JSON endpoints (see "Google provider").
- `deepl.rs`: DeepL over its web JSON-RPC endpoints (see "DeepL provider").
- `yandex.rs`: Yandex over the unofficial mobile endpoint (see "Yandex provider").
- `browser.rs` (native): the three services in the user's browser (see "Browser provider").

## Contracts and invariants
- `translate_texts` returns exactly one result per input text, in input order, when the
  provider-level setup succeeds. The outer `Err` is a setup failure that prevents the whole
  batch (unsupported language, web build).
- Empty or whitespace-only texts answer `Ok(String::new())` without a request.
- `MtItemErrorKind` tells the run loop how to react: `Failed` (that text failed, the run goes
  on), `Cancelled` (Stop; the run ends as cancelled), `CoolingDown { retry_in }` (the service is
  throttled longer than the call may wait; the run stops with `RunRateLimited`). Every message
  is localized; `MtItemError::from_acquire` maps a refused permit (`Busy` becomes `Failed`).
  `ServiceUnavailable` (browser transport only): no later text can be served either (extension
  not connected / outdated / disconnected, site cannot be driven); the run applies the batch's
  other results, then fails with the message.
- Once `call.is_cancelled()`, or an acquire returned `Cancelled` / `CoolingDown` (or a request
  ended `ServiceUnavailable`), a provider sends nothing more and answers that same error for
  every remaining text. The run loop still
  applies every `Ok` / `Failed` result of the batch before it stops the run.
- `preferred_batch_size` is how many consecutive bubbles the run loop hands one call: Google 1,
  DeepL 10, Yandex 8. A provider that packs texts must still map every answer back to its own
  text exactly, and fail a whole request's texts when it cannot.
- Signals: a rate-limit answer is `RateLimited`, a block / quota answer is `Blocked` (both with
  the server's `Retry-After`; Yandex's quota codes use the service cap instead), a normal answer is `Ok`, anything else (network, other status,
  bad format) is `Neutral`. The status is classified before the body is parsed, so a non-JSON
  429 still counts.
- Backends never mutate canvas / project / UI state and spawn no worker lifecycles of their own.
- No credentials here. If a provider ever needs one, thread it through explicit settings and
  never log it. Logs carry host, status, lengths and body kind, never the text or body content.

## Google provider (`google.rs`)
- Request: one `POST <endpoint>?client=..&sl=..&tl=..&ie=UTF-8&oe=UTF-8` per text, the text in
  the form field `q` (POST avoids Google's ~16 KB URL limit). Redirects are disabled, so a
  redirect to `google.com/sorry` (CAPTCHA) or `consent.google.com` is classified, never followed.
- Endpoint chain (`ENDPOINTS`), each on its limiter route: `translate.googleapis.com` and
  `translate.google.com` `/translate_a/single` (client `gtx`, `dt=t`; both on
  `MtRoute::GoogleGtx`, measured to be blocked together), then
  `clients5.google.com/translate_a/t` (client `dict-chrome-ex`, `MtRoute::GoogleClients5`).
  Any failure moves to the next endpoint. A cooling route is skipped WITHOUT a request (Google
  routes never wait a cooldown out), so a block on gtx sends the next texts to `clients5` at the
  service spacing. Cooldowns are 5, 5, 10, 20, 30 min (then 30); a route needs 3 successful
  probes to count as healthy again. Stop (`Cancelled`) or `Busy` ends the walk.
- When every endpoint failed and every Google route cools, the text is `CoolingDown` (the run
  stops); otherwise the most informative error is reported (400 / unsupported language >
  block > 429 > consent > format > other status > network > skipped route).
- Classification (`classify_reply`): 403, a `/sorry` redirect, or a 2xx / 429 body with a block
  page marker (`BLOCK_PAGE_MARKERS`: the observed `<title>Sorry...</title>` 429 page, "unusual
  traffic", "automated queries", CAPTCHA) is `Blocked`; a bare 429 is `RateLimited`; 400 means
  the endpoint / client id was rejected (not the text length).
- Endpoint memory: `GoogleMtSession` (one per run) remembers where the next walk starts and
  advances only past endpoints that failed for endpoint-specific reasons (400, other status,
  format, consent, network). Limit failures never move it: route health is the limiter's, so a
  gtx route leaving its cooldown is probed again instead of the session staying on `clients5`.
- Long texts: a text over the service's `max_request_chars` (5000) is cut by
  `mt_limits::split::split_for_request` into several requests (`translate_text`); each chunk
  keeps its own surrounding whitespace, blank chunks are not sent, and the translations are
  joined with the original separators. The text fails as a whole if any chunk fails.
- Parsing: `translate_a/single` = concatenation of `v[0][i][0]` over all segments, detected
  source `v[2]`; `translate_a/t` = `["text"]`, `"text"` or `[["text","lang"]]`. A blank result
  for non-blank input is a format failure (Google echoes untranslatable text).
- Languages: `GOOGLE_LANGUAGES` lists the codes the MT panel offers, in Google's spelling
  (`zh-cn` -> `zh-CN`). A code outside it is the outer `Err`, never sent: Google answers an
  unknown `tl` with `200` and an untranslated text.
- Errors: `GoogleError` (limiter refusals as `GoogleError::Limiter`), localized under
  `translation.mt.google.*`.
- Tests drive the chain through the `GoogleHttp` seam over a fake-clock limiter and pin
  `UreqHttp`'s redirect / status / `Retry-After` mapping on a loopback socket.

## DeepL provider (`deepl.rs`)
- Flow (adapted from translatepy): one batch of texts costs TWO JSON-RPC requests —
  `LMT_split_into_sentences` with every text, then one `LMT_handle_jobs` with the jobs of every
  sentence (context kept inside each text); translations are mapped back by per-text sentence
  counts. A sentence DeepL left untranslated fails only its text; an answer that cannot be
  mapped fails the whole batch.
- Batching (`pack_batches`): at most `DEEPL_BATCH_MAX_TEXTS` (10) texts and
  `DEEPL_BATCH_MAX_CHARS` (1000 chars) per batch; a longer single text goes alone, uncut.
- No `getClientState` and no other request: the JSON-RPC endpoint answers without it, and every
  extra request spends DeepL's per-client budget. JSON-RPC ids: each `DeeplClient` starts at a
  random `NNNN0001` baseline (`initial_request_id`) and increments per request.
- Agent: every session uses the one process-wide `ureq::Agent` (`shared_agent`, a
  `OnceLock` built once through `http::build_agent`; read-only after initialisation, its only
  state is the agent's own connection pool and cookie jar). Translation runs and the Cleaning
  prompt translator (`DeeplMtBackend`, a fresh session per call) therefore reuse a warm TLS
  connection instead of paying a handshake per run.
- Budget: every request takes a `MtRoute::Deepl` permit. DeepL counts requests per client
  (about 6-7 from idle, then about one per minute, whatever the spacing), so the limiter gives
  the route a request budget of 6 tokens and one more per 60 s and no spacing; a long run waits
  for tokens (reported in the MT panel) instead of failing, and Stop interrupts the wait.
- Rate limits: HTTP 429 or any JSON-RPC code `10429xx` (whatever the status; 1042901 / 1042902
  and 1042911 / 1042912 were observed) are `RateLimited`. The request is never re-sent
  (`rate_limit_retries` 0): requests sent while limited escalated the block from minutes to more
  than 20 min. The batch, and every later text of the call, ends `CoolingDown` with the
  cooldown the limiter started (2 / 5 / 10 / 15 min, escalating within 30 min of the last
  recovery), so the run stops with "try again in N min"; a new run during the cooldown is
  refused before any request.
- Seam: `DeeplHttp::post_json` (`UreqDeeplHttp` over `shared_agent`, redirects followed);
  tests script a fake one over a fake-clock limiter.
- Errors localized under `translation.mt.deepl.*`.

## Yandex provider (`yandex.rs`)
- Flow: in `auto` mode every text is first detected (`/detect`, one request each), then the
  texts go to `/translate`, packed by `plan_requests`: consecutive texts of ONE source
  language, at most `BATCH_TEXTS` (8) texts and `BATCH_MAX_CHARS` (5000 chars) per request (a
  longer single text goes alone). The endpoint answers one `text` per `text` form field, in
  order; a count mismatch fails every text of that request.
- Every request (detect and translate) takes a `MtRoute::Yandex` permit (service spacing 1 s;
  cooldowns 30 s, 60 s, 2 min, 5 min, waited out in place up to 60 s, longer = `CoolingDown`).
- Classification (`classify_reply`): HTTP 429 -> `RateLimited` (before parsing); JSON codes
  403 / 404 / 408 (daily request, daily / monthly character limits) -> `Blocked` with
  `retry_after` = the service's cap (10 min), so the next request is refused at once and the
  run stops instead of waiting out and re-probing an exhausted quota; a full success -> `Ok`;
  anything else -> `Neutral`.
- Seam: `YandexHttp` (`get`, `post_form`; `UreqYandexHttp` over `http::build_agent`, Android
  app User-Agent). A run keeps one `YandexSession` (one pooled agent); `YandexMtBackend` builds
  one per call.
- UCID: `cached_ucid`, a process-wide cache with a 360 s TTL (translatepy-compatible); it costs
  no request and no permit.
- Errors localized under `translation.mt.yandex.*`.

## Browser provider (`browser.rs`)
- Transport: the extension's `translate` module, version 1 (contract:
  `browser-extension/src/modules/translate/MODULE_README.md`), through the process-wide bridge
  started by `ms_browser_bridge::app::ensure_started_for_app` (the one builder of the app's
  bridge config) through `start_app_bridge`, this crate's ONE start path (platform gate +
  registered app version; also used by the tab's extension controller and, on a worker via
  `start_app_bridge_in_background`, by the startup's eager start
  `mt_transport::start_browser_bridge_if_used` and a toggle to the browser transport). Seam: `BrowserTranslateChannel` (`check_ready`, `translate`); production
  `BridgeChannel`, tests a scripted fake over a fake-clock limiter. The platform gate
  (`PlatformFeature::BrowserExtension`) is applied here.
- Packing (decision D2): line breaks inside a bubble become one space; texts are packed one per
  line, in order, up to 10 (DeepL) / 20 (Google, Yandex) texts and the browser lane's
  `max_request_chars` (newlines counted); a longer text goes alone, cut by
  `mt_limits::split::split_for_request_in` and joined back. Every size is in UTF-16 code units
  (`BROWSER_SIZE_UNIT`), the unit of the extension's `too_long` check, and the lane limits stay
  below the extension's site limits (mirror test reading `sites/<site>.ts` and `index.ts`).
  Blank texts answer `""` unsent. The answer must have exactly one non-empty line per text
  (CRLF / CR / NBSP normalized), else the batch is re-sent one text per request; a packed batch
  refused `too_long` is re-sent the same way. Each re-sent text checks Stop first; texts answered
  before a Stop keep their translations in the call's results (the run loop currently discards
  a cancelled call's results, see `../MODULE_README.md`). One permit of the service's browser
  route per request.
- Connection: checked once per session, before its first request, without a permit
  (`check_ready` + `await_ready_peer`). When the bridge was NOT listening before the check (the
  check started it, or a start is still binding) or has been listening for less than
  `READY_WAIT` (35 s: the extension's reconnect backoff caps at 30 s), it waits for a serving
  extension until the listener is `READY_WAIT` old, polling every 250 ms and honouring Stop; a
  detached call waits at most what is left of its 30 s. The listen start is this crate's first
  sighting of `Listening` (`LISTENING_SINCE` via `note_listen_state`: the startup / toggle start
  worker, `check_ready`, the extension panel's status read; the bridge API has no start time).
  An older listener with a missing or outdated extension fails the call at once; a listener
  failure or a peer without the module version never waits. Nothing extra is shown in the UI
  during the wait.
- Errors: remote `rate_limited` -> `RateLimited`, remote / app-side `attention_timeout` ->
  `Blocked` (the cooled lane then makes the rest of the call `CoolingDown`); not connected /
  outdated / disconnected / start failure and the remote codes `unknown_module`,
  `unknown_method`, `bad_params`, `site_changed`, `unsupported_language` ->
  `ServiceUnavailable`; `cancelled` -> `Cancelled`; the rest -> `Failed` (`Neutral`).
- Timings: a run's request 90 s plus attention states up to 10 min each (the user solving a
  captcha in the tab; reported through `MtCall::report_attention`); the detached call 30 s for
  everything. Stop is the run's cancel flag, which the bridge sees within 250 ms; a user Stop
  is logged at info level, not as a failure.
- Languages: each service's normalizer (`normalize_google_lang`, `normalize_deepl_lang`,
  `normalize_yandex_lang`, re-exported `pub(crate)` from their files) owns the spelling; DeepL's
  is lower-cased for its web page.
- Errors localized under `translation.mt.browser.*`, with the bridge's `browser_bridge.*` texts
  as the reason.

## Editing map
- Change a limit, spacing, cooldown step or retry count: `../mt_limits/table.rs`, never a
  provider.
- Change timeouts, redirect handling or `Retry-After` reading: `http.rs`.
- Add a provider: implement `MachineTranslatorBackend` (permit per request, typed signals) in a
  new file with a `run_session()`, export it from `mod.rs`, add an `MtService` variant (the
  exhaustive matches in `machine_translation.rs`, `mod.rs::run_session` and
  `../mt_limits/table.rs` then name every place), and its UI text in
  `panels/machine_translation.rs`.
- Google endpoints, fallback order, classification, block markers or languages: `google.rs`
  (`ENDPOINTS`, `classify_reply`, `BLOCK_PAGE_MARKERS`, `GOOGLE_LANGUAGES`); a new language in
  the MT panel needs a row in `GOOGLE_LANGUAGES` (a test checks both panel lists).
- DeepL JSON-RPC flow, batching, rate-limit classification or the shared agent: `deepl.rs`
  (the budget and cooldown numbers: `../mt_limits/table.rs`).
- Yandex request packing, detection or error mapping: `yandex.rs`.
- Browser transport packing, error meaning, timeouts or the module version it speaks:
  `browser.rs` (the module contract: `browser-extension/src/modules/translate/`).
- Provider language normalization: the provider's `normalize_*_lang` helper.
- The backend contract: `MachineTranslatorBackend`, every provider and
  `machine_translation.rs` (`PlainMtRun`, `translate_texts_via_translator`) together.
