# Module: src/backend_ipc

## Purpose
Rust side of the Rust <-> Python AI-backend IPC. Provides a framed, multiplexed
request/response + event client (`BackendClient`) over a pluggable byte transport.
Every Rust subsystem reaches the backend through this module (usually via the
process-wide `shared_client()`); no other code opens its own backend connection.

## Architecture
Two layers, cleanly separated:

1. Frame codec (`frame.rs`) — the wire format
   `[u32 BE header_len][header_json][u32 BE blob_len][blob]`. `header_json` is a
   UTF-8 JSON object; `blob` is raw binary. Size guards (`protocol::MAX_*`) are
   enforced before allocation. `read_frame` uses fill-to-length loops, so it does
   NOT assume one transport read == one frame.

2. Pluggable transport (`transport.rs`) — carries the codec bytes:
   - `Inner::Unix`: AF_UNIX `UnixStream` (default on Linux/Windows).
   - `Inner::Ws`: loopback WebSocket. A dedicated I/O thread OWNS the
     `tungstenite::WebSocket<TcpStream>`; app code never touches the socket. It
     exchanges bytes through `WsShared`: an inbound `VecDeque<u8>` byte queue
     (Condvar-signalled) and an outbound `mpsc` channel of whole buffers the I/O
     thread sends as WS BINARY messages. The single I/O thread multiplexes read and
     write on one WebSocket by using a short poll read timeout
     (`WS_IO_POLL_INTERVAL`); the caller's app-level read timeout is enforced via the
     inbound Condvar, never on the TCP socket.

Data flow (per request): `BackendClient::begin_call` -> `write_frame` on the shared
write half -> transport -> Python. The background `reader_loop` (`client.rs`) decodes
frames with `read_frame` and demultiplexes by correlation `id` (responses/progress ->
the waiting caller; `event{id:0}` -> per-topic subscribers). Reconnect is transparent
on the next `call` after the reader observes EOF/error.

Transport selection: `current_backend_endpoint()` returns `Unix(backend_socket_path())`
on unix and the WS endpoint published via `set_ws_endpoint()` on windows. The backend
supervisor (a different module) parses the backend's `MS_BACKEND_WS_PORT=<port>` line
and calls `set_ws_endpoint(port, token)`.

## Files and submodules
- `mod.rs`: module root and re-exports (`BackendClient`, `CallError`, `CallHandle`,
  `shared_client`, `Frame`, `read_frame`, `write_frame`, `backend_socket_path`, and —
  native only — `BackendEndpoint`, `set_ws_endpoint`, `current_backend_endpoint`).
- `protocol.rs`: constants mirroring `modules/ai_backend/ipc/protocol.py` (version,
  kinds, statuses, topics, method names, header keys) + header builders. Values must
  match Python byte-for-byte. Edit here when the shared contract changes.
- `frame.rs`: the frame codec (`Frame`, `read_frame`, `write_frame`). Edit here for
  wire-format / size-guard changes.
- `transport.rs`: connection primitives — `BackendStream` (Read/Write/clone/shutdown),
  `BackendEndpoint`, `connect_path`/`connect_ws`/`connect_endpoint`, the WS I/O thread
  (`ws_io_loop`, `WsShared`, `WsHandle`), and the process-global WS endpoint holder.
  Edit here to change how connections are opened, timed out, or torn down.
- `client.rs`: `BackendClient`, `CallHandle`, reader thread, id demux, reconnect, event
  subscriptions, `shared_client()`. A `#[cfg(wasm32)]` stub mirrors the public surface
  and returns clear errors (no backend on web). Edit here for routing/lifecycle logic.

## Contracts and invariants
- Wire bytes are identical on both transports; the receiver treats all payloads as one
  ordered byte stream and delimits frames by the length prefixes (WS: do NOT assume one
  WS message == one frame).
- Shutdown -> EOF contract: `BackendStream::shutdown()` on any clone must make a blocked
  `read()` on a SIBLING clone return `Ok(0)`. The reader thread's clean exit depends on
  this. Unix relies on `shutdown(Both)`; WS sets `closed`, wakes the inbound Condvar,
  drops the outbound sender, and shuts the retained TCP clone down to unblock the I/O
  thread.
- One `write` == one whole buffer (frame). The WS `write` returns `buf.len()` and never
  fragments a frame across WS messages; frame atomicity against concurrent writers still
  comes from the `write_half` mutex in `client.rs`.
- The auth token is never logged (only the WS port). WS handshake never enables TLS
  (loopback only) so the x86_64-pc-windows-gnu target keeps building.
- The GUI thread never blocks here: all backend I/O runs on the reader/I-O worker
  threads; callers use `call`/`begin_call` with timeouts.

## Editing map
- To change the wire format or size guards, see `frame.rs` (+ `protocol.rs` guards) and
  keep `modules/ai_backend/ipc` in sync.
- To add/adjust a method, topic, or header key, see `protocol.rs` (mirror Python).
- To change how a connection is opened, timed out, or shut down, see `transport.rs`.
- To add a new transport variant, extend `BackendEndpoint` + `Inner` and match all arms
  (no `_ =>` on the project enums), then wire it into `connect_endpoint`.
- To change request routing, reconnect, or subscriptions, see `client.rs`.
- The WS endpoint is published from the backend supervisor via `set_ws_endpoint`.
