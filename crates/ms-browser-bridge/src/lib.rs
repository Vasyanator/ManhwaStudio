/*
File: crates/ms-browser-bridge/src/lib.rs

Purpose:
Crate root of `ms-browser-bridge`: the app side of the channel to the ManhwaStudio browser
extension. A GUI-free, feature-agnostic loopback WebSocket server with pairing through
`bridge.json`, a mutually authenticated HMAC handshake, capability announcement (module name +
version), blocking request / response / cancel with events, and `reload`.

Main responsibilities:
- `protocol`: the wire protocol (one Rust owner; mirrored by
  `browser-extension/src/core/protocol.ts`).
- `auth`: the secret and the HMAC tags.
- `api`: config, status, peer, request / event types and the routing rule.
- `errors`: `BridgeError` with localized text (`browser_bridge.*`).
- `pairing` (native): the folder owner lock, `bridge.json` read / atomic write / trust and
  permission check, secret and port reuse.
- `server` (native): listener, connection threads, pending requests.
- `service` (native): the process-wide `BridgeService`.
- `app` (native): the ONE builder of the app's `BridgeConfig` from `ms-browser-ext` facts,
  the auto re-unpack of a stale install and the stale-peer reload watcher.

Contract:
- It knows no feature: modules and methods are strings, params / results opaque JSON. Feature
  semantics (machine translation, downloads) live in the consumers.
- Native-only server: the listener, the threads and the OS randomness are compiled out on wasm.
- Platform gating (`PlatformFeature::BrowserExtension`) is the caller's. The app's
  `BridgeConfig` (folder, id, bundle `build_id`) is built in ONE place, `app`, from
  `ms-browser-ext`; tests build their own config.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// The crate is named `browser_bridge`; type names like `BridgeError` repeat it on purpose.
#![allow(clippy::module_name_repetitions)]

// `t!` / `tf!` for the localized `BridgeError` text; `#[macro_export]`ed by ms-i18n.
#[macro_use]
extern crate ms_i18n;

pub mod api;
// The app's wiring (config from `ms-browser-ext`, auto re-unpack, reload watcher).
#[cfg(not(target_arch = "wasm32"))]
pub mod app;
pub mod auth;
pub mod errors;
pub mod protocol;

#[cfg(not(target_arch = "wasm32"))]
mod pairing;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(not(target_arch = "wasm32"))]
pub mod service;

#[cfg(test)]
mod mirror_tests;
#[cfg(test)]
mod test_support;

pub use api::{BridgeConfig, BridgeEvent, BridgeRequest, BridgeStatus, BridgeTimings, ListenState, PeerInfo, ReloadTarget, RequestOptions};
pub use errors::BridgeError;
pub use protocol::{CONFIG_FILE_NAME, ErrorCode, EventKind, PROTOCOL_VERSION, PeerState};
#[cfg(not(target_arch = "wasm32"))]
pub use service::BridgeService;
