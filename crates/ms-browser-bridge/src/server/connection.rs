/*
File: crates/ms-browser-bridge/src/server/connection.rs

Purpose:
One extension connection, on its own thread: the guarded HTTP upgrade, the challenge / hello /
welcome handshake, then the frame loop (outbound queue, responses, events, ping / pong,
silence drop) until the peer leaves or the server shuts down.

Key items:
- `serve` — the thread body.
- `upgrade_refusal` — the pure path / Host / Origin rule (403 before any frame).
- `evaluate_hello` — the pure handshake decision (reject / ready / outdated).

Notes:
- The socket has a read timeout of `timings.poll`, so `read` doubles as the loop's tick: a
  timeout is "no data", the outbound queue is drained every tick.
- The whole upgrade + handshake must finish within `timings.handshake`.
- An outdated peer (other `PROTOCOL_VERSION`) only gets `pong` / `reload`; its other frames are
  ignored, because their shapes are unknown.
- Binary frames are refused with close 1003 (reserved for the future chunk format, see
  `protocol.rs`); text over `MAX_TEXT_MESSAGE_BYTES` ends the connection with close 1009.
- Events (`deliver_event`): an `event` message over `MAX_EVENT_MESSAGE_BYTES` ends the connection
  with close 1009; with the request's event budget used up (`server/mod.rs`, "Bounded queues")
  a `progress` event is dropped and an attention event ends the connection with close 1008. The
  thread never blocks on a requester.
- Never logs the secret, the nonces' tags or request params / results.
*/

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::time::Instant;

use ms_log::runtime_log;
use serde_json::Value;
use tungstenite::handshake::HandshakeError;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use super::{EVENT_QUEUE_CAPACITY, OUTBOUND_CAPACITY, PeerSlot, Reply, Shared, lock};
use crate::api::{BridgeEvent, PeerInfo};
use crate::auth;
use crate::errors::BridgeError;
use crate::protocol::{
    AppMessage, ChallengeMsg, EventKind, EventMsg, ExtMessage, HelloCore, HelloDetails, HelloMsg, MAX_EVENT_MESSAGE_BYTES, MAX_TEXT_MESSAGE_BYTES, PROTOCOL_VERSION, PeerState, RejectCode,
    RejectMsg, ResponseMsg, WS_PATH, WelcomeMsg,
};

type Socket = WebSocket<TcpStream>;

/// Longest header value quoted in a log line (they are attacker-controlled).
const LOG_VALUE_MAX_CHARS: usize = 120;

/// The thread body of one accepted connection. Every failure is logged here; nothing escapes.
pub(super) fn serve(shared: &Arc<Shared>, stream: TcpStream) {
    let timings = shared.cfg.timings;
    let configured = stream
        .set_nodelay(true)
        .and_then(|()| stream.set_read_timeout(Some(timings.poll)))
        .and_then(|()| stream.set_write_timeout(Some(timings.write)));
    if let Err(err) = configured {
        runtime_log::log_warn(format!("[browser_bridge] could not configure a connection socket: {err}"));
        return;
    }
    let deadline = Instant::now() + timings.handshake;
    let Some(mut ws) = upgrade(shared, stream, deadline) else { return };
    let Some((nonce, hello)) = challenge_and_read_hello(shared, &mut ws, deadline) else { return };
    match evaluate_hello(shared, &nonce, &hello) {
        HelloVerdict::Reject { code, message } => {
            runtime_log::log_warn(format!("[browser_bridge] handshake rejected ({code:?}): {message}"));
            if send(&mut ws, &AppMessage::Reject(RejectMsg { code, message: message.to_owned() })) {
                close(&mut ws, CloseCode::Policy, "handshake rejected");
            }
        }
        HelloVerdict::Accept { info, client_nonce } => run_peer(shared, ws, info, &nonce, &client_nonce),
    }
}

/// Performs the guarded upgrade; `None` (logged) when refused, failed or too slow.
fn upgrade(shared: &Arc<Shared>, stream: TcpStream, deadline: Instant) -> Option<Socket> {
    let port = shared.port;
    let allowed_origin = shared.cfg.allowed_origin.clone();
    // The `Result<Response, ErrorResponse>` shape (and the size of tungstenite's `ErrorResponse`)
    // is fixed by tungstenite's `Callback` trait; it cannot be boxed here.
    #[allow(clippy::result_large_err, reason = "signature imposed by tungstenite::handshake::server::Callback")]
    let check = move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let header = |name: &str| request.headers().get(name).and_then(|value| value.to_str().ok());
        match upgrade_refusal(request.uri().path(), request.uri().query(), header("host"), header("origin"), port, &allowed_origin) {
            None => Ok(response),
            Some(reason) => {
                runtime_log::log_warn(format!(
                    "[browser_bridge] refused upgrade ({reason}): path={:?} host={:?} origin={:?}",
                    clip(request.uri().path()),
                    header("host").map(clip),
                    header("origin").map(clip)
                ));
                let mut refusal = ErrorResponse::new(Some("forbidden".to_owned()));
                *refusal.status_mut() = StatusCode::FORBIDDEN;
                Err(refusal)
            }
        }
    };
    let config = WebSocketConfig::default().max_message_size(Some(MAX_TEXT_MESSAGE_BYTES)).max_frame_size(Some(MAX_TEXT_MESSAGE_BYTES));
    let mut attempt = tungstenite::accept_hdr_with_config(stream, check, Some(config));
    loop {
        match attempt {
            Ok(ws) => return Some(ws),
            Err(HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= deadline || shared.is_shut_down() {
                    runtime_log::log_warn("[browser_bridge] upgrade not completed in time; connection dropped");
                    return None;
                }
                attempt = mid.handshake();
            }
            Err(HandshakeError::Failure(err)) => {
                // A refused upgrade already got its 403 (and was logged by `check`).
                if !matches!(err, tungstenite::Error::Http(_)) {
                    runtime_log::log_warn(format!("[browser_bridge] upgrade failed: {err}"));
                }
                return None;
            }
        }
    }
}

/// The upgrade rule: `None` when the request may become a bridge connection, else why not.
/// Only `GET <WS_PATH>` without a query, `Host: 127.0.0.1:<port>` (DNS-rebinding guard) and
/// `Origin: <allowed_origin>` exactly.
pub(super) fn upgrade_refusal(path: &str, query: Option<&str>, host: Option<&str>, origin: Option<&str>, port: u16, allowed_origin: &str) -> Option<&'static str> {
    if path != WS_PATH || query.is_some() {
        return Some("wrong path");
    }
    if host != Some(format!("127.0.0.1:{port}").as_str()) {
        return Some("wrong host");
    }
    if origin != Some(allowed_origin) {
        return Some("wrong origin");
    }
    None
}

fn clip(value: &str) -> String {
    value.chars().take(LOG_VALUE_MAX_CHARS).collect()
}

/// Sends `challenge` and waits for the first text frame. `None` (logged) on any failure.
fn challenge_and_read_hello(shared: &Shared, ws: &mut Socket, deadline: Instant) -> Option<(String, String)> {
    let nonce = match auth::generate_nonce() {
        Ok(nonce) => nonce,
        Err(err) => {
            runtime_log::log_error(format!("[browser_bridge] OS random source failed: {err}"));
            close(ws, CloseCode::Error, "internal error");
            return None;
        }
    };
    if !send(ws, &AppMessage::Challenge(ChallengeMsg { protocol: PROTOCOL_VERSION, nonce: nonce.clone() })) {
        return None;
    }
    loop {
        if Instant::now() >= deadline || shared.is_shut_down() {
            runtime_log::log_warn("[browser_bridge] no hello within the handshake deadline; connection dropped");
            close(ws, CloseCode::Policy, "handshake timeout");
            return None;
        }
        match read(ws) {
            ReadOutcome::Idle | ReadOutcome::Message(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            ReadOutcome::Message(Message::Text(text)) => return Some((nonce, text.as_str().to_owned())),
            ReadOutcome::Message(Message::Binary(_)) => {
                refuse_binary(ws);
                return None;
            }
            ReadOutcome::Message(Message::Close(_)) | ReadOutcome::Closed => return None,
            ReadOutcome::Failed(err) => {
                log_read_failure(ws, None, &err);
                return None;
            }
        }
    }
}

/// The handshake decision.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HelloVerdict {
    /// Send `reject { code, message }` and close 1008.
    Reject { code: RejectCode, message: &'static str },
    /// Authenticated: register `info` (its `state` says ready / outdated) and send `welcome`.
    Accept { info: PeerInfo, client_nonce: String },
}

/// Decides the handshake for the first text frame `text`, given the connection's `nonce`.
/// Reads only the frozen fields first, so a peer of another protocol version can still be
/// authenticated and welcomed as `outdated` (D1).
pub(super) fn evaluate_hello(shared: &Shared, nonce: &str, text: &str) -> HelloVerdict {
    let malformed = HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, message: "malformed hello" };
    let Ok(value) = serde_json::from_str::<Value>(text) else { return malformed };
    if value.get("type").and_then(Value::as_str) != Some("hello") {
        return HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, message: "expected hello" };
    }
    let Ok(core) = serde_json::from_value::<HelloCore>(value.clone()) else { return malformed };
    if !auth::is_valid_nonce(&core.client_nonce) || !auth::verify_client_auth(&shared.secret, nonce, &core.client_nonce, &core.auth) {
        return HelloVerdict::Reject { code: RejectCode::AuthFailed, message: "authentication failed" };
    }
    let (details, modules, state) = if core.protocol == PROTOCOL_VERSION {
        let Ok(full) = serde_json::from_value::<HelloMsg>(value) else { return malformed };
        let HelloMsg { build_id, ext_version, browser, modules, .. } = full;
        (HelloDetails { build_id, ext_version, browser }, modules, PeerState::Ready)
    } else {
        // Another protocol: only the frozen display fields are read, leniently.
        (serde_json::from_value::<HelloDetails>(value).unwrap_or_default(), Vec::new(), PeerState::Outdated)
    };
    let build_current = shared.cfg.expected_build_id.as_ref().is_none_or(|expected| details.build_id.as_ref() == Some(expected));
    let info = PeerInfo {
        id: shared.next_peer_id(),
        browser: details.browser,
        ext_version: details.ext_version,
        build_id: details.build_id,
        protocol: core.protocol,
        modules,
        state,
        build_current,
    };
    HelloVerdict::Accept { info, client_nonce: core.client_nonce }
}

/// Registers the peer, sends `welcome` and runs the frame loop until the peer leaves.
fn run_peer(shared: &Arc<Shared>, mut ws: Socket, info: PeerInfo, nonce: &str, client_nonce: &str) {
    let (outbound_tx, outbound_rx) = mpsc::sync_channel(OUTBOUND_CAPACITY);
    let peer = info.id;
    let state = info.state;
    runtime_log::log_info(format!(
        "[browser_bridge] peer {peer} connected: {} {} ext {} protocol {} build {:?} ({}) state {state:?} modules [{}]",
        info.browser.name,
        info.browser.version,
        info.ext_version,
        info.protocol,
        info.build_id,
        if info.build_current { "current" } else { "differs from the app's" },
        info.modules.iter().map(|m| format!("{}@{}", m.name, m.version)).collect::<Vec<_>>().join(", ")
    ));
    // Registered BEFORE `welcome`: a request queued in between waits in the outbound channel,
    // which this thread drains only after `welcome` went out.
    lock(&shared.peers).push(PeerSlot { info, outbound: outbound_tx });
    let _registration = Registration { shared, peer };
    let welcome = WelcomeMsg {
        protocol: PROTOCOL_VERSION,
        app_version: shared.cfg.app_version.clone(),
        expected_build_id: shared.cfg.expected_build_id.clone(),
        state,
        server_auth: auth::server_auth(&shared.secret, nonce, client_nonce),
    };
    if !send(&mut ws, &AppMessage::Welcome(welcome)) {
        return;
    }
    frame_loop(shared, &mut ws, peer, state, &outbound_rx);
}

/// Unregisters a peer and fails its pending requests when its thread ends (any path).
struct Registration<'a> {
    shared: &'a Shared,
    peer: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        // Order matters (see `server/mod.rs`, "Disconnect race"): unregister, THEN drain.
        lock(&self.shared.peers).retain(|slot| slot.info.id != self.peer);
        let orphaned: Vec<_> = {
            let mut pending = lock(&self.shared.pending);
            let ids: Vec<u64> = pending.iter().filter(|(_, entry)| entry.peer == self.peer).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| pending.remove(&id)).collect()
        };
        for entry in orphaned {
            // Removed above, so this is the entry's only terminal reply (its slot is free).
            match entry.reply.try_send(Reply::Disconnected) {
                Ok(()) => {}
                // The requester already gave up (its receiver is gone): nothing to tell.
                Err(TrySendError::Disconnected(_)) => runtime_log::log_info(format!("[browser_bridge] peer {}: a pending requester already left", self.peer)),
                Err(TrySendError::Full(_)) => runtime_log::log_error(format!("[browser_bridge] peer {}: reply queue unexpectedly full; disconnect not delivered", self.peer)),
            }
        }
        runtime_log::log_info(format!("[browser_bridge] peer {} disconnected", self.peer));
    }
}

fn frame_loop(shared: &Shared, ws: &mut Socket, peer: u64, state: PeerState, outbound: &Receiver<AppMessage>) {
    let silence = shared.cfg.timings.silence;
    let mut last_seen = Instant::now();
    loop {
        if shared.is_shut_down() {
            close(ws, CloseCode::Away, "app shutting down");
            return;
        }
        // Stops at `Empty` (nothing more this tick) or `Disconnected` (cannot happen while the slot
        // is registered).
        while let Ok(message) = outbound.try_recv() {
            if !send(ws, &message) {
                return;
            }
        }
        match read(ws) {
            ReadOutcome::Idle => {}
            ReadOutcome::Message(message) => {
                last_seen = Instant::now();
                match message {
                    Message::Text(text) => {
                        let keep = match state {
                            PeerState::Ready => handle_ready_text(shared, ws, peer, text.as_str()),
                            PeerState::Outdated => handle_outdated_text(ws, peer, text.as_str()),
                        };
                        if !keep {
                            return;
                        }
                    }
                    Message::Binary(_) => {
                        refuse_binary(ws);
                        return;
                    }
                    // tungstenite queues the pong to a WS ping itself; flushed below.
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    Message::Close(_) => {
                        flush(ws);
                        return;
                    }
                }
            }
            ReadOutcome::Closed => return,
            ReadOutcome::Failed(err) => {
                log_read_failure(ws, Some(peer), &err);
                return;
            }
        }
        if last_seen.elapsed() >= silence {
            runtime_log::log_warn(format!("[browser_bridge] peer {peer} silent for {silence:?}; dropped"));
            close(ws, CloseCode::Policy, "silence timeout");
            return;
        }
        flush(ws);
    }
}

/// Handles one text frame of a ready peer; `false` ends the connection.
fn handle_ready_text(shared: &Shared, ws: &mut Socket, peer: u64, text: &str) -> bool {
    match serde_json::from_str::<ExtMessage>(text) {
        Ok(ExtMessage::Ping) => send(ws, &AppMessage::Pong),
        Ok(ExtMessage::Pong) => true,
        Ok(ExtMessage::Response(response)) => {
            deliver_response(shared, peer, response);
            true
        }
        Ok(ExtMessage::Event(event)) => match deliver_event(shared, peer, event, text.len()) {
            EventVerdict::Keep => true,
            EventVerdict::Close { code, reason } => {
                close(ws, code, reason);
                false
            }
        },
        Ok(ExtMessage::Hello(_)) => {
            runtime_log::log_warn(format!("[browser_bridge] peer {peer} sent a second hello; dropped"));
            close(ws, CloseCode::Policy, "unexpected hello");
            false
        }
        Err(err) => {
            let value = serde_json::from_str::<Value>(text).ok();
            let kind = value.as_ref().and_then(|v| v.get("type")).and_then(Value::as_str).map(clip);
            runtime_log::log_warn(format!("[browser_bridge] peer {peer} sent an invalid message (type {kind:?}): {err}"));
            // A malformed response must not leave its requester waiting for the deadline.
            if kind.as_deref() == Some("response")
                && let Some(id) = value.as_ref().and_then(|v| v.get("id")).and_then(Value::as_u64)
            {
                fail_pending(shared, peer, id, BridgeError::Protocol { detail: err.to_string() });
            }
            true
        }
    }
}

/// An outdated peer: answer its `ping` (frozen shape), ignore everything else.
fn handle_outdated_text(ws: &mut Socket, peer: u64, text: &str) -> bool {
    let kind = serde_json::from_str::<Value>(text).ok().and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned));
    match kind.as_deref() {
        Some("ping") => send(ws, &AppMessage::Pong),
        Some("pong") => true,
        other => {
            runtime_log::log_info(format!("[browser_bridge] outdated peer {peer}: ignored message type {:?}", other.map(clip)));
            true
        }
    }
}

/// What `deliver_event` decided about the connection.
#[derive(Debug, PartialEq, Eq)]
enum EventVerdict {
    /// The event was queued, dropped by policy or ignored; the peer stays.
    Keep,
    /// The peer broke an event limit: close with `code`.
    Close { code: CloseCode, reason: &'static str },
}

/// Queues `event` (its text message was `message_len` bytes) for its requester without blocking,
/// applying the limits of the file header. Only `peer`'s own requests are served.
fn deliver_event(shared: &Shared, peer: u64, event: EventMsg, message_len: usize) -> EventVerdict {
    let id = event.id;
    if message_len > MAX_EVENT_MESSAGE_BYTES {
        runtime_log::log_warn(format!("[browser_bridge] peer {peer}: event of {message_len} bytes for request {id} exceeds {MAX_EVENT_MESSAGE_BYTES}; dropped the peer"));
        return EventVerdict::Close { code: CloseCode::Size, reason: "event too large" };
    }
    let target = lock(&shared.pending).get(&id).filter(|entry| entry.peer == peer).map(|entry| (entry.reply.clone(), Arc::clone(&entry.budget)));
    let Some((reply, budget)) = target else {
        runtime_log::log_info(format!("[browser_bridge] peer {peer}: event for unknown or finished request {id}"));
        return EventVerdict::Keep;
    };
    // Only this thread adds to `queued` for this request, so check-then-add cannot overshoot.
    if budget.queued.load(Ordering::Acquire) >= EVENT_QUEUE_CAPACITY {
        return match event.kind {
            EventKind::Progress => {
                budget.dropped_progress.fetch_add(1, Ordering::Relaxed);
                EventVerdict::Keep
            }
            // Attention transitions drive the requester's deadline: they are never dropped, and a
            // peer that floods them is not a working extension.
            EventKind::Attention | EventKind::AttentionCleared => {
                runtime_log::log_warn(format!("[browser_bridge] peer {peer}: request {id} has {EVENT_QUEUE_CAPACITY} unread events and got another attention event; dropped the peer"));
                EventVerdict::Close { code: CloseCode::Policy, reason: "event budget exceeded" }
            }
        };
    }
    budget.queued.fetch_add(1, Ordering::AcqRel);
    match reply.try_send(Reply::Event(BridgeEvent { request_id: id, kind: event.kind, data: event.data })) {
        Ok(()) => {}
        Err(TrySendError::Disconnected(_)) => {
            budget.queued.fetch_sub(1, Ordering::AcqRel);
            runtime_log::log_info(format!("[browser_bridge] peer {peer}: event for request {id} after its requester left"));
        }
        Err(TrySendError::Full(_)) => {
            // Cannot happen: the channel holds EVENT_QUEUE_CAPACITY events + the one terminal reply,
            // and `queued` was below the capacity.
            budget.queued.fetch_sub(1, Ordering::AcqRel);
            runtime_log::log_error(format!("[browser_bridge] peer {peer}: reply queue of request {id} unexpectedly full; event dropped"));
        }
    }
    EventVerdict::Keep
}

fn deliver_response(shared: &Shared, peer: u64, response: ResponseMsg) {
    let id = response.id;
    let result = if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        match response.error {
            Some(error) => {
                let retry_after = error.retry_after();
                Err(BridgeError::Remote { code: error.code, message: error.message, retry_after })
            }
            None => Err(BridgeError::Protocol { detail: "response with ok=false carries no error".to_owned() }),
        }
    };
    complete_pending(shared, peer, id, result);
}

fn fail_pending(shared: &Shared, peer: u64, id: u64, error: BridgeError) {
    complete_pending(shared, peer, id, Err(error));
}

/// Removes request `id` (only if `peer` serves it) and hands its requester `result`.
fn complete_pending(shared: &Shared, peer: u64, id: u64, result: Result<Value, BridgeError>) {
    let entry = {
        let mut pending = lock(&shared.pending);
        if pending.get(&id).is_some_and(|entry| entry.peer == peer) { pending.remove(&id) } else { None }
    };
    match entry {
        // The entry was removed above, so this is its only terminal reply and its slot is free.
        Some(entry) => match entry.reply.try_send(Reply::Response(result)) {
            Ok(()) => {}
            Err(TrySendError::Disconnected(_)) => runtime_log::log_info(format!("[browser_bridge] peer {peer}: response for request {id} after its requester left")),
            Err(TrySendError::Full(_)) => runtime_log::log_error(format!("[browser_bridge] peer {peer}: reply queue of request {id} unexpectedly full; response lost")),
        },
        None => runtime_log::log_info(format!("[browser_bridge] peer {peer}: response for unknown or finished request {id}")),
    }
}

/// One `read` outcome, with the poll timeout folded into `Idle`.
enum ReadOutcome {
    Message(Message),
    Idle,
    Closed,
    Failed(tungstenite::Error),
}

fn read(ws: &mut Socket) -> ReadOutcome {
    match ws.read() {
        Ok(message) => ReadOutcome::Message(message),
        Err(tungstenite::Error::Io(err)) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => ReadOutcome::Idle,
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => ReadOutcome::Closed,
        Err(err) => ReadOutcome::Failed(err),
    }
}

fn log_read_failure(ws: &mut Socket, peer: Option<u64>, err: &tungstenite::Error) {
    let who = peer.map_or_else(|| "handshake".to_owned(), |peer| format!("peer {peer}"));
    runtime_log::log_warn(format!("[browser_bridge] {who}: connection failed: {err}"));
    if matches!(err, tungstenite::Error::Capacity(_)) {
        close(ws, CloseCode::Size, "message too large");
    }
}

fn refuse_binary(ws: &mut Socket) {
    runtime_log::log_warn("[browser_bridge] binary frame refused (not supported in protocol 1)");
    close(ws, CloseCode::Unsupported, "binary frames are not supported");
}

/// Serializes and sends `message`; `false` (logged) when the connection is no longer usable.
fn send(ws: &mut Socket, message: &AppMessage) -> bool {
    let text = match serde_json::to_string(message) {
        Ok(text) => text,
        Err(err) => {
            // Plain structs and string-keyed maps cannot fail to serialize; logged, not sent.
            runtime_log::log_error(format!("[browser_bridge] could not serialize an outgoing message: {err}"));
            return true;
        }
    };
    match ws.send(Message::text(text)) {
        Ok(()) => true,
        Err(err) => {
            runtime_log::log_warn(format!("[browser_bridge] send failed: {err}"));
            false
        }
    }
}

/// Flushes queued frames (e.g. tungstenite's automatic pongs); a timeout keeps them queued.
fn flush(ws: &mut Socket) {
    match ws.flush() {
        Ok(()) | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {}
        Err(tungstenite::Error::Io(err)) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
        Err(err) => runtime_log::log_info(format!("[browser_bridge] flush failed: {err}")),
    }
}

/// Starts the closing handshake with `code` and pushes the close frame out; the socket is
/// dropped by the caller right after (the peer's close reply is not awaited).
fn close(ws: &mut Socket, code: CloseCode, reason: &'static str) {
    match ws.close(Some(CloseFrame { code, reason: reason.into() })) {
        Ok(()) => flush(ws),
        Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {}
        Err(err) => runtime_log::log_info(format!("[browser_bridge] close failed: {err}")),
    }
}
