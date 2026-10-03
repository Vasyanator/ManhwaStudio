/*
File: crates/ms-backend-ipc/src/lib.rs

Purpose:
Crate root of `ms-backend-ipc` — the Rust <-> Python AI-backend IPC layer. Hosts the
single framed transport and re-exports everything the application imports as
`backend_ipc::*` (the binary mounts this crate under that name through a re-export
shim in `src/main.rs`).

Submodules:
- `transport`: dual-transport connection primitives (`backend_socket_path`,
  `connect_path`/`connect_ws`/`connect_endpoint`, `BackendStream`,
  `BackendEndpoint`, the process-global WS endpoint holder, and the
  process-global per-root socket-name holder used by `--ignore-installed`).
- `protocol`: Rust mirror of `modules/ai_backend/ipc/protocol.py` — protocol
  version, kind/topic/method/header constants.
- `textdetector`: the forward-only text-detector wire contract (`ForwardEngine`,
  `build_forward_request`, `decode_forward_response`, `max_tiles_per_request`); pure
  functions over bytes and JSON, no I/O.
- `frame`: the frame codec (`Frame`, `read_frame`, `write_frame`) implementing
  the `[u32 BE header_len][header_json][u32 BE blob_len][blob]` wire format.
- `client`: the framed, multiplexed `BackendClient` (background reader thread,
  id demultiplexing, hello handshake, reconnect, event subscriptions), the
  process-wide `shared_client()` accessor, and the diagnostic-only host-version
  seed (`set_studio_version` / `studio_version`).

Transport:
The framed codec runs over a pluggable transport selected per platform by
`transport::current_backend_endpoint`: AF_UNIX on unix, the token-authenticated
loopback WebSocket endpoint published by the supervisor on windows. The frame bytes are
identical on both transports.
*/

#![warn(clippy::all)]

// The user-visible strings this crate emits (backend connection/handshake failures) go
// through the `ms-i18n` macros. They are `#[macro_export]`ed and expand via `$crate::`,
// so one crate-root `#[macro_use]` is all a consumer crate needs — the same single line
// `src/main.rs` uses for the binary.
#[macro_use]
extern crate ms_i18n;

pub mod client;
pub mod frame;
pub mod protocol;
pub mod textdetector;
pub mod transport;

// `backend_socket_path` is the single source of truth for the IPC socket path,
// used by the launcher/settings call sites via `backend_ipc::backend_socket_path()`.
// `seed_isolated_backend_socket_name` is the startup-only knob that makes that path
// unique per runtime root (`--ignore-installed`); it must be called before any
// backend client or supervisor exists.
pub use transport::{backend_socket_path, seed_isolated_backend_socket_name};

// Dual-transport endpoint API. The backend supervisor publishes the loopback WS
// endpoint via `set_ws_endpoint`; `current_backend_endpoint` selects the transport
// per platform. Native-only: the wasm build has no backend process/transport.
// `#[allow(unused_imports)]` matches the re-exports below: the supervisor consumer of
// `set_ws_endpoint` may not be linked in every build configuration yet.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
pub use transport::{BackendEndpoint, current_backend_endpoint, set_ws_endpoint};

// Re-export the most-used framed entry points at the module root.
#[allow(unused_imports)]
pub use client::{BackendClient, CallError, CallHandle, shared_client};
// The host application's own version, which this crate cannot read for itself
// (`env!("CARGO_PKG_VERSION")` here yields the LIBRARY's `0.1.0`). The binary seeds it at
// startup; it is logged at every handshake and is diagnostic only — never on the wire.
#[allow(unused_imports)]
pub use client::{set_studio_version, studio_version};
#[allow(unused_imports)]
pub use frame::{Frame, read_frame, write_frame};
