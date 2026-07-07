# Module: modules/ai_backend/ipc

## Purpose
Python implementation of the framed, multiplexed, bidirectional IPC protocol between the Rust
frontend (client) and the Python AI backend (server). The default byte transport is a single
AF_UNIX domain socket (`frame_server.py`); a token-authenticated WebSocket transport
(`frame_ws_server.py`) is an interchangeable fallback for environments without AF_UNIX. The legacy
HTTP server has been removed.

The authoritative wire specification is `PROTOCOL.md` in this directory.

## Architecture
The package is layered: `framing` owns the wire codec, `events` owns server-push fan-out,
`registry` owns the handler lookup table, `dispatcher` owns the per-connection read loop, and the
listener layer has two interchangeable transports — `frame_server` (AF_UNIX, default) and
`frame_ws_server` (WebSocket fallback). Both feed the SAME `serve_connection`/`Dispatcher`/handler
stack; only the byte transport differs. Handlers live in the `handlers/` sub-package, one module
per feature group, self-registering into `registry.METHOD_HANDLERS` at import time.

```
frame_server.py          — AF_UNIX listener + worker pool + EventBus construction
frame_ws_server.py       — WebSocket (TCP) listener; token-authed handshake, same stack
    └── dispatcher.py    — per-connection read loop, hello handshake, cancel registry
        ├── framing.py   — [u32 BE header_len][header_json][u32 BE blob_len][blob] codec
        ├── events.py    — EventBus: fan-out event{id:0} frames to all live connections
        ├── protocol.py  — shared constants (kind/topic/method/status names, version, guards)
        └── registry.py  — METHOD_HANDLERS dict + register() + HandlerContext + imports handlers/
            └── handlers/ — one module per group; each self-registers at import time
                ├── health.py      — health (pull) + TOPIC_HEALTH push via health worker
                ├── ocr.py         — ocr.manga / ocr.easy / ocr.paddle / ocr.paddle_vl / ocr.surya / ocr.paddle_onnx
                ├── textdetector.py— textdetector.ctd / .paddle / .surya
                ├── inpaint.py     — inpaint.lama_v2 / .lama_mpe / .aot (+ unloads)
                ├── sdxl.py        — inpaint.sdxl (+ unload); streaming via ProgressEmitter
                ├── reline.py      — reline.models / reline.process
                ├── device.py      — device.get / .set / .cuda_diagnostics
                └── translate.py   — translate.deep
```

## Wire format (summary — `PROTOCOL.md` is authoritative)

Every message in either direction is a single frame:

```
[u32 BE header_len][header_json (UTF-8)][u32 BE blob_len][blob (raw bytes)]
```

- `header_json` carries all structured fields (`v`, `id`, `kind`, `method`, `topic`, `status`,
  inline request params, inline result fields).
- `blob` carries raw binary data (PNG image bytes). Never base64. May be zero-length.
- Size guards: header ≤ 1 MiB, blob ≤ 32 MiB.

## Message kinds

| Kind       | Direction        | Role                                                               |
|------------|------------------|--------------------------------------------------------------------|
| `hello`    | client→server, server→client | Handshake on connect; carries `v`=1 and (server reply) `backend_version`. |
| `request`  | client→server    | One RPC call; `id` ≥ 1, `method` names the handler.              |
| `progress` | server→client    | Zero or more intermediate frames before `response` (SDXL streaming). |
| `response` | server→client    | Terminal frame for a `request`; `status`: `ok`/`error`/`interrupted`. |
| `cancel`   | client→server    | Request cancellation by `id`; unknown/finished ids are a no-op.   |
| `event`    | server→client    | Unsolicited push; `id`=0, `topic` identifies the payload type.    |
| `error`    | server→client    | Protocol-level error (framing/version/unknown kind); not a request result. |

## Event topics

| Topic        | Trigger                             | Key payload fields                                          |
|--------------|-------------------------------------|-------------------------------------------------------------|
| `health`     | Periodic (~1 s) snapshot push       | `ok`, `service`, `backend_version`, `is_torch_available`, per-service objects |
| `device`     | Device/provider selection changed   | Same shape as `device.get` response                         |
| `model_load` | Model load/unload progress (opt-in) | `model`, `phase`, `loaded`, `total`, `message`              |
| `log`        | Backend log line stream (opt-in)    | `level`, `message`, `ts_unix_s`                             |

`TOPIC_HEALTH` replaces health polling. Rust subscribes after the hello handshake via
`BackendClient::subscribe(TOPIC_HEALTH)` and folds events into the shared health snapshot.

## Key components

### `framing.py`
Pure wire codec. `read_frame(reader)` → `(header: dict, blob: bytes)`. `write_frame(writer,
header, blob)` serializes one frame atomically. `FrameWriteLock` serializes concurrent writer
threads per connection. `encode_frame` builds the bytes without I/O (used by `EventBus` to encode
once and fan the same bytes to all subscribers).

### `events.py` — `EventBus`
Thread-safe fan-out of `event{id:0}` frames to all registered `EventSink`s. Each sink is a
`(writer, write_lock, sock)` triple; the sock enables a per-write send timeout so a slow/dead
client is dropped rather than stalling the publisher for all other connections.

### `registry.py` — `HandlerContext`, `METHOD_HANDLERS`, `register`
`HandlerContext` carries `state` (shared `AppState`), `events` (`EventBus`), `get_health_snapshot`,
and a per-request `progress_emitter` (injected by the dispatcher; `None` for non-streaming
handlers). `register(method, handler)` populates `METHOD_HANDLERS`; the decorator form
`@register(METHOD_X)` is equivalent. Importing this module also imports the `handlers/` package,
triggering all self-registrations.

### `dispatcher.py` — `Dispatcher`
Per-connection read loop. Performs the `hello` handshake, routes `request` frames to a shared
`ThreadPoolExecutor`, tracks per-id `cancel` events, and serializes all outbound frames through
`FrameWriteLock`. `ProgressEmitter` (one per in-flight request, injected via `dataclasses.replace`
on a per-request context copy) lets streaming handlers push `progress{id}` frames safely under
concurrent load. Per-connection in-flight cap is 32 requests.

### `frame_server.py` — `FrameUnixServer`, `run_frame_server`
`FrameUnixServer` (`ThreadingMixIn + UnixStreamServer`) binds the base backend socket path with
single-instance safety (live peer → `FrameBackendInstanceError`, stale file → unlink), `chmod
0o600`, and unlink-on-close. `run_frame_server(state, socket_path, stop_event, ...)` builds the
shared `EventBus`, worker pool, and `HandlerContext`, then serves until `stop_event` is set.

### `frame_ws_server.py` — `FrameWsServer`, `run_frame_ws_server`
WebSocket fallback transport (Python is the WS server, Rust the WS client). `FrameWsServer`
(`ThreadingMixIn + TCPServer`) binds `(ws_host, ws_port)` (port 0 → ephemeral). Requires `wsproto`
(pure-Python; pulls `h11`). Per connection, `_WsStreamAdapter` drives a `wsproto` SERVER handshake:
it extracts the `token` query param from the handshake target and `hmac.compare_digest`s it against
`ws_token` — mismatch/missing → `RejectConnection` (HTTP 401), match → `AcceptConnection`. After the
upgrade the adapter exposes the `read(n)`/`write(data)`/`flush()` duck-typed stream `framing.py`
needs: inbound WS BINARY payloads are concatenated into ONE ordered byte stream (length prefixes
delimit frames; one WS message == one frame is NOT assumed), PING→PONG, CLOSE/EOF → `read` returns
`b""`. `serve_connection(..., sock=None)` is used (the raw TCP socket must not reach the event bus:
a per-write `settimeout` would corrupt WS framing). `run_frame_ws_server(...)` mirrors
`run_frame_server` but prints `MS_BACKEND_WS_PORT=<bound_port>` (flushed) for the Rust supervisor
and has no AF_UNIX/unlink/chmod handling.

Outbound concurrency model: each connection runs ONE dedicated writer thread that is the sole
caller of `WSConnection.send` + `socket.sendall` after the handshake; response/progress/event
frames (`write`), PONG replies, and the CLOSE echo are all ENQUEUED onto a bounded per-connection
queue instead of being sent inline. This keeps the socket single-writer (no interleaved WS frames).
The read thread's `receive_data`/`events` still shares the one non-thread-safe `WSConnection` with
the writer's `send`, so a small `_ws_lock` guards every `WSConnection` access; it is held only
around the non-blocking encode/parse, never across a blocking `recv`/`sendall`. Slow/dead-client
isolation (the AF_UNIX path gets from `EventSink`'s socket timeout) is provided here by the bounded
queue: a `write`/enqueue that stays blocked on a full queue past `_OUTBOUND_PUT_TIMEOUT_S` (2 s,
matching `events._PUBLISH_WRITE_TIMEOUT_S`) tears the connection down and raises `BrokenPipeError`,
so a wedged peer can never stall the publisher for everyone else.

### `handlers/`
One module per feature group; each calls `registry.register(METHOD_X, handler_fn)` at module level
so the act of importing the package wires everything. The shared touch-point for adding a new group
is a single import line in `handlers/__init__.py`.

## Contracts and invariants
- The frame protocol version is 1 (`PROTOCOL_VERSION`). A client with a different `v` is rejected
  at handshake with a `kind:"error"` frame before any request.
- Image bytes are never base64-encoded on the wire. Request blobs carry raw PNG input; response
  blobs carry raw PNG output (masks, inpaint results, SDXL previews).
- Inpaint methods that need two images (image + mask) use a concatenated blob: `blob = image_png ++
  mask_png` with `image_len`/`mask_len` header fields splitting them.
- A `cancel{id}` sets that id's `threading.Event`; the handler observes it and raises `Interrupted`
  to emit `response{status:"interrupted"}`.
- Event fan-out is best-effort. A broken or slow sink is dropped silently; the publisher never
  raises. Slow-client isolation uses a 2 s per-write socket timeout (`_PUBLISH_WRITE_TIMEOUT_S`).
- Handlers must not import `server.py` directly; they receive `AppState` via `HandlerContext.state`.
- Do not add imports directly to `registry.py`. Add one line to `handlers/__init__.py` only.

## Editing map
- Wire format or protocol constants: `framing.py` and `protocol.py`; update `PROTOCOL.md` first.
- Event bus fan-out behavior or slow-client isolation: `events.py`.
- Handshake, cancel registry, in-flight cap, or progress emitter wiring: `dispatcher.py`.
- AF_UNIX listener, single-instance safety, or worker pool: `frame_server.py`.
- WebSocket transport, handshake token check, or WS byte-stream adapter: `frame_ws_server.py`.
- Handler for an existing feature group: the matching `handlers/<group>.py`.
- New feature group: create `handlers/<group>.py`, add one import line to `handlers/__init__.py`,
  and add the new method to `PROTOCOL.md §5`.
