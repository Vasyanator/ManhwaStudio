/*
File: backend_ipc/mod.rs

Purpose:
Directory-module root for the Rust <-> Python AI-backend IPC. Hosts the single
framed transport and re-exports everything the rest of the crate imports as
`backend_ipc::*`.

Submodules:
- `transport`: AF_UNIX socket primitives (`backend_socket_path`, `connect_path`,
  `BackendStream`).
- `protocol`: Rust mirror of `modules/ai_backend/ipc/protocol.py` — protocol
  version, kind/topic/method/header constants.
- `frame`: the frame codec (`Frame`, `read_frame`, `write_frame`) implementing
  the `[u32 BE header_len][header_json][u32 BE blob_len][blob]` wire format.
- `client`: the framed, multiplexed `BackendClient` (background reader thread,
  id demultiplexing, hello handshake, reconnect, event subscriptions) plus the
  process-wide `shared_client()` accessor.

Transport:
The framed protocol is the single, sole IPC transport. Both Rust and Python bind
the base socket path ([`backend_socket_path`]); there is no second/legacy socket.
*/

pub mod client;
pub mod frame;
pub mod protocol;
pub mod transport;

// `backend_socket_path` is the single source of truth for the IPC socket path,
// used by the launcher/settings call sites via `backend_ipc::backend_socket_path()`.
pub use transport::backend_socket_path;

// Re-export the most-used framed entry points at the module root.
#[allow(unused_imports)]
pub use client::{BackendClient, CallError, CallHandle, shared_client};
#[allow(unused_imports)]
pub use frame::{Frame, read_frame, write_frame};
