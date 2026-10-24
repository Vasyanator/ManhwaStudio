# Module: browser-extension/

## Purpose
The ManhwaStudio browser extension (Chromium MV3, TypeScript). It runs in the user's own
browser and connects to the running app over an authenticated loopback WebSocket, so app
features can use the browser's session: machine translation through the translator web pages
first, later more modules (e.g. downloads). It is a general platform: the core knows nothing
about any feature; features are modules.

Its place in the project: the app embeds the built extension (`crates/ms-browser-ext`,
`build.rs`), unpacks it into `<data_dir>/browser_extension/chromium`, and the user loads that
folder once with "Load unpacked". The app side of the wire is `crates/ms-browser-bridge`.
It lives at the repo root, not under `crates/`, because the root `build.rs` watches `crates/`
recursively and would rescan `node_modules`.

## Architecture
```
background.ts (service worker)
  registerModule(...) for each feature -> core/modules.ts ModuleRegistry (sealed at start)
  BridgeConnection (core/connection.ts)
    bridge.json (no-store fetch) -> ws://127.0.0.1:<port>/ms-bridge
    challenge -> hello(HMAC) -> welcome(server HMAC) -> ready | outdated
    ready: request/cancel -> Dispatcher (core/dispatcher.ts) -> FeatureModule.handle(ctx)
           ctx.tabs = SiteTabManager (core/tabs.ts), ctx.attention = AttentionController
  wake-ups: runtime.onStartup / onInstalled / alarms (30 s)
```
- One connection per service-worker lifetime; reconnect backoff 1, 2, 4 ... 30 s; ping every
  20 s keeps the worker alive (Chrome >= 116); 60 s of silence drops the link.
- Requests are answered exactly once; the app owns deadlines and sends `cancel`.
- Site work happens only in the extension's own pinned, non-discardable tab per site, under a
  per-site exclusive lease; content scripts are injected programmatically into that tab only.

## Files and submodules
- `manifest.json`: MV3 manifest. `key` pins the extension id (`ms_browser_ext::CHROMIUM_EXTENSION_ID`
  is derived from it at build time); never change or remove it. The private key exists only
  in the gitignored repo-root `.secret/browser-extension-key.pem` and is not needed to build.
  Permissions are the minimum the current modules need; a new module asks for more through
  `optional_host_permissions` at runtime, never by widening the install-time set.
- `src/core/protocol.ts`: wire protocol constants, message types, guards. Mirror of
  `ms_browser_bridge::protocol` (a Rust test reads this file).
- `src/core/auth.ts`: handshake HMAC (WebCrypto), nonces.
- `src/core/connection.ts`: `BridgeConnection` state machine and its production seams.
- `src/core/dispatcher.ts`: request routing, cancel, events, error mapping.
- `src/core/modules.ts`: `FeatureModule`, `ModuleContext`, `BridgeFailure`, the registry.
- `src/core/tabs.ts`: `SiteTabManager`, `SiteSpec`, `SiteTabLease`, URL glob matching.
- `src/core/attention.ts`: `AttentionController` (captcha / challenge hand-over to the user).
- `src/background.ts`: service-worker entry; module registration lines go here.
- `src/modules/<name>/`: one feature module each, with its own `MODULE_README.md`:
  - `translate/` — module `translate` v1, method `translate` (Google / DeepL / Yandex web
    pages in a dedicated tab per site; one text per request -> `{text}`). Wire contract,
    per-site notes and what is unverified: `src/modules/translate/MODULE_README.md`.
- `protocol-fixtures/*.json`: one golden example of every message, `bridge_config.json` and
  `auth_vector.json`; shared with the Rust bridge tests.
- `test/*.test.ts`: `node --test` suites (bundled by `build.mjs --tests`); `test/helpers.ts`
  holds fixtures access and fakes of the browser seams.
  Module suites are named `test/<module>*.test.ts` (translate: content helpers, site tables,
  module flow over a fake page).
- `build.mjs`: type check + esbuild; `package.json` / `package-lock.json`: pinned dev tools.

## Contracts and invariants
- **Protocol versioning:** `PROTOCOL_VERSION` (protocol.ts) is the only compatibility gate;
  any change of a message, field meaning, closed set or handshake string bumps it here AND in
  `ms_browser_bridge::protocol`. `challenge`, `hello`, `welcome`, `reject`, `reload`, `ping`,
  `pong` are frozen forever (new optional fields only): the extension sends `hello` to an app
  of any protocol version and obeys `reload` while `outdated`.
- `build_id` (from the bundle's `build-info.json`, written by the app's build) is not a
  compatibility gate; the app uses it to re-unpack and send `reload`. A folder built by
  `npm run build` has no `build-info.json` and announces `"unknown"`.
- HMAC key = the 32 raw bytes of the 64-hex `secret` in `bridge.json`.
- Browser close codes: only 1000 or 3000-4999 are allowed, so the extension sends 4002 /
  4003 / 4008 for protocol error / unsupported data / policy.
- Module contract: register at top level before `bridge.start()`; one module = one request
  namespace (`request.module`); unknown methods reject `BridgeFailure("unknown_method")`;
  honour `ctx.signal`; emit events only for the running request. The future binary `chunk`
  stream (downloads) is reserved in protocol.ts and refused in v1.
- Content scripts: `src/modules/<name>/content/driver.ts` is bundled as an IIFE to
  `modules/<name>/driver.js`; the module injects it with `chrome.scripting.executeScript`.
- Site patterns (`SiteSpec.hostPatterns`, `challengeUrlPatterns`): globs over the full URL,
  `*` = any run of characters.
- The bundle never contains `bridge.json`, `build-info.json` (added by the app build) or
  `ms-extension-stamp.json`; `build.mjs` refuses to produce them.
- TypeScript: `strict`, no `any`, no unchecked casts; src sees only `chrome` types (tests get
  Node types through the generated `.test-out/tsconfig.test.json`).
- Tracked files follow the root `.gitignore` allowlist; `node_modules/`, `dist/`,
  `.test-out/` and `node_modules.lock` stay untracked.

## Build and verification
- `npm ci --ignore-scripts` once (the app's `build.rs` does it automatically when the
  lockfile hash changes), then `npm test` (type check src + tests, bundle, `node --test`).
- `node build.mjs --out <dir>` builds a loadable unpacked extension; it wipes `<dir>` only if
  it is empty or was created by `build.mjs` (marker `.ms-ext-out`).
- Node/npm are optional for building the app: without them the app builds without the
  extension (`ms_browser_ext::bundle()` -> `NotIncluded`). After installing node, touch
  `package.json` so cargo reruns the build script.

## Editing map
- New feature module: `src/modules/<name>/index.ts` exporting a `FeatureModule`, one
  `registerModule(...)` line in `background.ts`, tests in `test/<name>*.test.ts`, extra host
  permissions in `manifest.json` only if unavoidable.
- Wire change: `src/core/protocol.ts` + fixtures + `ms_browser_bridge::protocol`, bump
  `PROTOCOL_VERSION` in both.
- Reconnect / keep-alive policy: `src/core/connection.ts`.
- Dedicated-tab behaviour: `src/core/tabs.ts`; challenge hand-over: `src/core/attention.ts`.
