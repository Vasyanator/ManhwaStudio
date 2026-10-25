# Module: crates/ms-browser-bridge/src

## Purpose
The app side of the channel to the ManhwaStudio browser extension: a GUI-free, feature-agnostic
loopback WebSocket server. It pairs with the extension through `bridge.json`, authenticates both
sides with HMAC-SHA256, learns the extension's modules (name + version), and lets worker threads
call `module.method(params)` with cancel, deadline, attention suspension and events. It knows no
feature: machine translation (first consumer) and a future download module build their own
semantics on top.

## Architecture
```text
consumer worker thread ──request()──> BridgeService::shared() (service.rs, process-wide)
                                         └─ BridgeServer (server/mod.rs): peers + pending
listener thread (non-blocking accept) ──> one thread per connection (server/connection.rs)
   upgrade guard (path / Host / Origin, 403) -> challenge -> hello (HMAC) -> welcome
   -> frame loop: drain outbound AppMessages, read Ext messages, ping/pong, silence drop
```
- `ensure_started` spawns a start thread (owner lock + pairing file I/O + bind); `status()` is a
  brief-lock snapshot safe on the GUI thread.
- A requester registers a pending entry, queues `request` on the chosen peer's outbound channel
  and waits on its own reply channel in slices of at most 250 ms.
- Native-only: `pairing`, `server`, `service` are compiled out on wasm; `protocol`, `auth`,
  `api`, `errors` build everywhere.

## Files and submodules
- `lib.rs`: crate root, lint levels, re-exports.
- `protocol.rs`: THE wire protocol (constants, `BridgeFile`, `AppMessage` / `ExtMessage`,
  `ErrorCode`, `EventKind`, `PeerState`, the frozen-field rule, the reserved binary chunk format).
- `auth.rs`: `Secret`, nonces, `client_auth` / `server_auth` / `verify_client_auth`.
- `api.rs`: `BridgeConfig`, `BridgeTimings`, `BridgeStatus`, `PeerInfo`, `BridgeRequest`,
  `RequestOptions`, `BridgeEvent`, `ReloadTarget`, the routing rule `select_peer`.
- `errors.rs`: `BridgeError` (localized `browser_bridge.*`), `remote_code_text`.
- `pairing.rs`: the folder's inter-process owner lock (`.ms-bridge.lock`), `bridge.json` read /
  atomic write (`ms_atomic_file`, temp created 0600), trust check (regular file of the current user), mode 0600, secret reuse, port
  reuse with fallback.
- `server/mod.rs`: `BridgeServer` (start, status, request, reload, shutdown), listener loop.
- `server/connection.rs`: per-connection thread: upgrade guard, handshake decision, frame loop.
- `server/tests.rs`: loopback tests against a fake extension.
- `service.rs`: `BridgeService` (process-wide, a documented global §5 exception).
- `test_support.rs`, `mirror_tests.rs`: test-only helpers and the Rust <-> TS mirror guard.
- `app.rs` (native): the ONE place that builds the app's `BridgeConfig` from `ms-browser-ext`
  facts (`ensure_started_for_app`, every consumer's entry); the auto re-unpack of a stale
  install on the calling worker (`unpack_plan`, `install_guarded`, the one serialized unpack of
  the process; `ms_browser_ext::install` adds the inter-process lock); the stale-peer reload
  watcher (`ReloadLedger`: per peer connection, a lost reload retried after
  `RELOAD_RETRY_AFTER`, at most `MAX_RELOADS_PER_IDENTITY` per installation, only while the folder
  holds the embedded bundle); the registered app version (`register_app_version`).

## Contracts and invariants
- Protocol owner: `protocol.rs` + `browser-extension/src/core/protocol.ts`. Any change of a
  message, field or code meaning bumps `PROTOCOL_VERSION` in BOTH. `challenge`, `hello`'s
  `protocol` / `client_nonce` / `auth` / `build_id`, `welcome`, `reject`, `reload`, `ping`,
  `pong`, `bridge.json` and the HMAC construction are frozen forever.
- Compatibility: `PROTOCOL_VERSION` only. An authenticated peer of another protocol is welcomed
  `outdated` (only ping / pong / reload; requests fail `Outdated`). A bad HMAC is `reject` + close
  1008. `build_id` never gates: a differing peer stays `ready`, `PeerInfo::needs_reload` is true.
- Modules: a request goes to the OLDEST ready peer announcing the module at exactly the
  requested version, preferring one running the app's bundle (`build_current`): a stale peer is
  the one the reload watcher reloads, and it serves only when no current peer matches.
- One server per extension folder ACROSS processes: the owner lock is held by the listener
  thread for the listener's life and covers read -> secret -> bind -> write. A second app process
  reads `ListenState::Failed { OwnedByOtherProcess }`, never touches `bridge.json`, fails requests
  with that error, and takes over on a later `ensure_started` once the owner exited.
- `bridge.json` (unix): a foreign-owned or non-regular (symlink) file is never trusted; the final
  file is checked on every start (current user's regular file, mode 0600, repaired if wider).
- Security: bind `127.0.0.1` only; upgrade only on `GET /ms-bridge`, `Host: 127.0.0.1:<port>`,
  `Origin` = the configured extension origin; HMAC tags compared in constant time; mutual
  authentication (`server_auth`). The secret and request params / results are never logged.
- Limits: 5 s handshake, 60 s silence drop, 4 MiB text messages (close 1009 beyond), 64 KiB
  `event` messages (close 1009 beyond), binary frames refused with 1003, at most 16 connections.
- Bounded queues (`server/mod.rs` header): outbound per peer `OUTBOUND_CAPACITY` (non-blocking
  sends; full -> request `Disconnected`, cancel dropped, reload not queued); reply per request
  `EVENT_QUEUE_CAPACITY` events + one reserved terminal slot (a terminal response / disconnect is
  never dropped; surplus `progress` is dropped and counted; an attention event over the budget
  closes the peer 1008). Connection threads never block on a requester.
- Request ids: `1..=MAX_WIRE_ID` (2^53 - 1, JS-safe), wrapping, skipping ids still pending.
- Threading: locks (`peers`, `pending`, the service slot) are held only for collection updates
  and sender clones, never across I/O, a wait or `on_event`. `request` blocks: never call it on
  the GUI thread. A peer is unregistered BEFORE its pending requests are failed; a requester
  inserts its pending entry BEFORE re-checking its peer (no lost disconnect).
- Tests never call `BridgeService::shared()` and write only under `std::env::temp_dir()`.
- `app`: `build_id` never gates (D1); a folder without a stamp is never installed implicitly
  (installing is the user's action), a stale one is re-unpacked before the server starts; a
  started server writes `bridge.json` into the folder even when nothing is installed. Platform
  gating stays with the caller (this crate does not depend on `ms-platform`).

## Editing map
- New message or field: `protocol.rs` + `protocol.ts` + a fixture in
  `browser-extension/protocol-fixtures/`, bump `PROTOCOL_VERSION` in both.
- New remote error code: `ErrorCode` (+ `ALL`, `as_str`), `errors.rs::remote_code_text`, a
  `browser_bridge.remote.*` key (en + ru), `protocol.ts`, bump the protocol.
- Handshake / upgrade rules: `server/connection.rs` (`upgrade_refusal`, `evaluate_hello`).
- How the app configures, re-unpacks or reloads the extension: `app.rs`.
- Waiting semantics (deadline, attention, cancel): `server/mod.rs::wait_for_reply`.
- Binary chunk transfer (future download module): implement the reserved format documented in
  `protocol.rs` in `server/connection.rs`; bump the protocol.
