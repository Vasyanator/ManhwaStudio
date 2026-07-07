/*
File: backend_ipc/mod.rs

Purpose:
Directory-module root for the Rust <-> Python AI-backend IPC. Hosts the single
framed transport and re-exports everything the rest of the crate imports as
`backend_ipc::*`.

Submodules:
- `transport`: dual-transport connection primitives (`backend_socket_path`,
  `connect_path`/`connect_ws`/`connect_endpoint`, `BackendStream`,
  `BackendEndpoint`, and the process-global WS endpoint holder).
- `protocol`: Rust mirror of `modules/ai_backend/ipc/protocol.py` — protocol
  version, kind/topic/method/header constants.
- `frame`: the frame codec (`Frame`, `read_frame`, `write_frame`) implementing
  the `[u32 BE header_len][header_json][u32 BE blob_len][blob]` wire format.
- `client`: the framed, multiplexed `BackendClient` (background reader thread,
  id demultiplexing, hello handshake, reconnect, event subscriptions) plus the
  process-wide `shared_client()` accessor.

Transport:
The framed codec runs over a pluggable transport (AF_UNIX default; loopback
WebSocket fallback), selected per platform by `transport::current_backend_endpoint`
(AF_UNIX path on unix, published WS endpoint on windows). The frame bytes are
identical on both transports.
*/

pub mod client;
pub mod frame;
pub mod protocol;
pub mod transport;

// `backend_socket_path` is the single source of truth for the IPC socket path,
// used by the launcher/settings call sites via `backend_ipc::backend_socket_path()`.
pub use transport::backend_socket_path;

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
#[allow(unused_imports)]
pub use frame::{Frame, read_frame, write_frame};
