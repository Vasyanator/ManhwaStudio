/*
File: crates/ms-browser-bridge/src/server/mod.rs

Purpose:
One loopback bridge server instance: the listener thread, the peer registry, the pending
request table and the blocking `request` used by worker threads. Tests start their own
instance on a temp folder; the app goes through the process-wide `BridgeService` (`service.rs`).

Key items:
- `BridgeServer::{start, status, request, request_reload, shutdown}`.
- `Shared` — state shared by the listener, connection threads and requesters.
- `Reply` — what a connection thread hands a waiting requester; requesters hand a connection
  thread plain `AppMessage`s on the peer's outbound channel.
- `EventBudget` — the per-request count of queued events (bounded reply queue).

Threading:
- `start` blocks (owner lock + pairing file I/O + bind): call it on a worker. The folder's
  inter-process owner lock (`pairing::OwnerLock`) lives in the listener thread and is released
  right after the listening socket closes.
- One listener thread (non-blocking accept polled every `timings.poll`), one thread per
  connection (`connection.rs`), at most `MAX_CONNECTIONS` at once.
- A requester never touches a socket: it queues an `AppMessage` on the peer's outbound channel and waits on
  its own `Reply` channel in slices of at most `CANCEL_SLICE`.
- Locks (`peers`, `pending`) are held only to look up / insert / remove entries and clone
  senders; never across I/O, a channel wait or a callback. They are never nested.
- Disconnect race: a connection thread removes its peer from `peers` BEFORE draining its
  `pending` entries; a requester inserts its pending entry BEFORE re-checking that the peer is
  still registered. Either the requester sees the peer gone, or the drain sees its entry.

Bounded queues (nothing a peer sends can grow memory without bound):
- A peer's outbound channel holds at most `OUTBOUND_CAPACITY` messages; senders never block
  (`try_send`). Full means the connection thread is stuck in a write (it is dropped after
  `timings.write`): a request then fails `Disconnected`, a cancel is dropped (the extension's own
  `timeout_ms` bounds the abandoned work), a reload is reported as not queued (the reload watcher
  retries).
- A request's reply channel holds `EVENT_QUEUE_CAPACITY` events plus the ONE terminal reply
  (`Response` or `Disconnected`; both are sent only after the pending entry was removed, so at
  most one is ever sent). The connection thread never blocks on it: with the event budget used
  up, a `progress` event is dropped (counted, logged once by the requester) and an attention event
  ends the peer's connection (budget exceeded). Events larger than
  `protocol::MAX_EVENT_MESSAGE_BYTES` end the connection too (`connection.rs`).

Request ids: `1..=protocol::MAX_WIRE_ID` (JS-safe), wrapping, never one still pending.
*/

mod connection;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ms_log::runtime_log;
use serde_json::Value;

use crate::api::{BridgeConfig, BridgeEvent, BridgeRequest, BridgeStatus, ListenState, PeerInfo, ReloadTarget, RequestOptions, select_peer};
use crate::auth::Secret;
use crate::errors::BridgeError;
use crate::pairing;
use crate::protocol::{AppMessage, CancelMsg, EventKind, MAX_WIRE_ID, RequestMsg};

/// Longest single wait of a blocked requester: bounds how late a cancel, a deadline or a
/// disconnect is noticed.
pub(crate) const CANCEL_SLICE: Duration = Duration::from_millis(250);

/// Upper bound on simultaneous connections (authenticated or not); extra sockets are closed at
/// once. A browser profile opens one; several browsers a few.
const MAX_CONNECTIONS: usize = 16;

/// Capacity of a peer's outbound queue. Only the app fills it (requests, cancels, reloads: a few
/// per worker), and the connection thread drains it every `timings.poll`.
pub(crate) const OUTBOUND_CAPACITY: usize = 64;

/// Events one request may have queued and not yet taken by its requester.
pub(crate) const EVENT_QUEUE_CAPACITY: usize = 32;

/// Per-request bookkeeping of the bounded reply queue, shared by the connection thread (adds) and
/// the requester (takes).
#[derive(Debug, Default)]
pub(crate) struct EventBudget {
    /// Events sent to the reply channel and not yet received by the requester.
    pub(crate) queued: AtomicUsize,
    /// `progress` events dropped because the queue was full.
    pub(crate) dropped_progress: AtomicU64,
}

/// What a connection thread delivers to a waiting requester.
#[derive(Debug)]
pub(crate) enum Reply {
    /// The final answer (remote failures already mapped to `BridgeError::Remote`).
    Response(Result<Value, BridgeError>),
    /// A progress / attention notification.
    Event(BridgeEvent),
    /// The serving peer disconnected.
    Disconnected,
}

/// A registered (authenticated) peer.
#[derive(Debug)]
pub(crate) struct PeerSlot {
    pub(crate) info: PeerInfo,
    pub(crate) outbound: SyncSender<AppMessage>,
}

/// A request waiting for its response.
#[derive(Debug)]
pub(crate) struct Pending {
    /// `PeerInfo::id` of the peer serving it; only that peer may answer it.
    pub(crate) peer: u64,
    pub(crate) reply: SyncSender<Reply>,
    /// The event budget of this request (see the file header, "Bounded queues").
    pub(crate) budget: Arc<EventBudget>,
}

/// State shared by the listener thread, the connection threads and requesters.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) cfg: BridgeConfig,
    pub(crate) secret: Secret,
    pub(crate) port: u16,
    pub(crate) shutdown: AtomicBool,
    connections: AtomicUsize,
    next_request_id: AtomicU64,
    next_peer_id: AtomicU64,
    pub(crate) peers: Mutex<Vec<PeerSlot>>,
    pub(crate) pending: Mutex<HashMap<u64, Pending>>,
}

impl Shared {
    /// A fresh peer id (connection order).
    pub(crate) fn next_peer_id(&self) -> u64 {
        self.next_peer_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn is_shut_down(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    /// Registers a pending request for `peer` under a fresh id (see [`allocate_request_id`]).
    fn register_pending(&self, entry: Pending) -> u64 {
        let mut pending = lock(&self.pending);
        let id = allocate_request_id(&self.next_request_id, &pending);
        pending.insert(id, entry);
        id
    }
}

/// The next request id from `counter`: in `1..=MAX_WIRE_ID` (wrapping to 1 after the top), and
/// never one of `pending`'s keys. Terminates: `pending` is tiny next to the id space.
pub(crate) fn allocate_request_id(counter: &AtomicU64, pending: &HashMap<u64, Pending>) -> u64 {
    loop {
        // The closure always returns `Some`, so both arms carry the previous value.
        let (Ok(id) | Err(id)) = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| Some(if id >= MAX_WIRE_ID { 1 } else { id + 1 }));
        // A counter that was ever set out of range is brought back into it.
        let id = if (1..=MAX_WIRE_ID).contains(&id) { id } else { 1 };
        if !pending.contains_key(&id) {
            return id;
        }
    }
}

/// Locks `mutex`, recovering from poisoning: every critical section here is a single insert /
/// remove / clone on a collection, which a panic elsewhere cannot leave half-applied.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned: PoisonError<MutexGuard<'_, T>>| {
        runtime_log::log_warn("[browser_bridge] recovered a poisoned lock");
        poisoned.into_inner()
    })
}

/// A running loopback bridge server. Dropping it shuts it down (threads exit within one poll).
#[derive(Debug)]
pub(crate) struct BridgeServer {
    shared: Arc<Shared>,
}

impl BridgeServer {
    /// Takes the folder's owner lock, settles the pairing file, binds `127.0.0.1` and spawns the
    /// listener thread (which holds the lock until its socket is closed).
    ///
    /// Blocking file I/O: worker threads only.
    ///
    /// # Errors
    /// `OwnedByOtherProcess` (another live server owns the folder) / `PairingIo` / `Bind` /
    /// `System` from the pairing step; `Bind` when the listener cannot be
    /// made non-blocking; `System` when the listener thread cannot be spawned.
    pub(crate) fn start(cfg: BridgeConfig) -> Result<Self, BridgeError> {
        let (listener, paired, owner) = pairing::bind_paired(&cfg.install_dir)?;
        listener.set_nonblocking(true).map_err(|err| BridgeError::Bind { detail: err.to_string() })?;
        let shared = Arc::new(Shared {
            cfg,
            secret: paired.secret,
            port: paired.port,
            shutdown: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            next_request_id: AtomicU64::new(1),
            next_peer_id: AtomicU64::new(1),
            peers: Mutex::new(Vec::new()),
            pending: Mutex::new(HashMap::new()),
        });
        let listener_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ms-bridge-listen".into())
            .spawn(move || {
                listen_loop(&listener_shared, &listener);
                // Socket first, then the lock: a next owner never sees the folder free while this
                // listener still accepts.
                drop(listener);
                drop(owner);
            })
            .map_err(|err| BridgeError::System { detail: err.to_string() })?;
        runtime_log::log_info(format!("[browser_bridge] listening on 127.0.0.1:{}", paired.port));
        Ok(Self { shared })
    }

    /// The bound loopback port.
    #[cfg(test)]
    pub(crate) fn port(&self) -> u16 {
        self.shared.port
    }

    /// Snapshot of the listener and the peers. Brief lock; GUI-safe.
    pub(crate) fn status(&self) -> BridgeStatus {
        let listen = if self.shared.is_shut_down() { ListenState::NotStarted } else { ListenState::Listening { port: self.shared.port } };
        let peers = lock(&self.shared.peers).iter().map(|slot| slot.info.clone()).collect();
        BridgeStatus { listen, peers }
    }

    /// Sends `reload` to the peers `target` selects; returns how many were addressed.
    pub(crate) fn request_reload(&self, target: ReloadTarget) -> usize {
        self.queue_reload(|info| target.includes(info)).len()
    }

    /// Sends `reload` to exactly the connected peers whose `PeerInfo::id` is in `ids`; returns
    /// the ids it was queued for (a peer that left, or whose queue is full, is missing).
    pub(crate) fn request_reload_peers(&self, ids: &[u64]) -> Vec<u64> {
        self.queue_reload(|info| ids.contains(&info.id))
    }

    fn queue_reload(&self, select: impl Fn(&PeerInfo) -> bool) -> Vec<u64> {
        let senders: Vec<(u64, SyncSender<AppMessage>)> =
            lock(&self.shared.peers).iter().filter(|slot| select(&slot.info)).map(|slot| (slot.info.id, slot.outbound.clone())).collect();
        let mut queued = Vec::with_capacity(senders.len());
        for (peer, outbound) in senders {
            match outbound.try_send(AppMessage::Reload) {
                Ok(()) => {
                    queued.push(peer);
                    runtime_log::log_info(format!("[browser_bridge] reload sent to peer {peer}"));
                }
                Err(TrySendError::Full(_)) => runtime_log::log_warn(format!("[browser_bridge] peer {peer}: outbound queue full; reload not queued")),
                Err(TrySendError::Disconnected(_)) => runtime_log::log_info(format!("[browser_bridge] peer {peer} left before its reload was queued")),
            }
        }
        queued
    }

    /// Stops accepting and closes every connection (asynchronously, within one poll).
    pub(crate) fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
    }

    /// Runs `req` on the oldest ready peer that announces the module, BLOCKING until the
    /// response, the deadline, a cancel or a disconnect. Worker threads only.
    ///
    /// The deadline (`opts.timeout`) is suspended while the extension reports an attention
    /// state, for at most `opts.attention_timeout` per attention state. Events go to
    /// `opts.on_event` on this thread with no lock held. On cancel / timeout / attention
    /// timeout a `cancel` is sent to the extension.
    ///
    /// # Errors
    /// Routing (`NotConnected`, `Outdated`, `CapabilityMissing`), `Cancelled`, `Timeout`,
    /// `AttentionTimeout`, `Disconnected`, `Protocol`, or the extension's `Remote` error.
    pub(crate) fn request(&self, req: &BridgeRequest<'_>, opts: &RequestOptions<'_>) -> Result<Value, BridgeError> {
        if is_cancelled(opts) {
            return Err(BridgeError::Cancelled);
        }
        let (peer, outbound) = {
            let peers = lock(&self.shared.peers);
            let infos: Vec<PeerInfo> = peers.iter().map(|slot| slot.info.clone()).collect();
            let index = select_peer(&infos, req.module, req.module_version)?;
            (peers[index].info.id, peers[index].outbound.clone())
        };
        // Events plus the one terminal reply (see the file header, "Bounded queues").
        let (reply_tx, reply_rx) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY + 1);
        let budget = Arc::new(EventBudget::default());
        let id = self.shared.register_pending(Pending { peer, reply: reply_tx, budget: Arc::clone(&budget) });
        let _pending = PendingGuard { shared: &self.shared, id };
        // See the file header ("Disconnect race"): re-check AFTER inserting the pending entry.
        if !lock(&self.shared.peers).iter().any(|slot| slot.info.id == peer) {
            return Err(BridgeError::Disconnected);
        }
        let message = AppMessage::Request(RequestMsg {
            id,
            module: req.module.to_owned(),
            method: req.method.to_owned(),
            params: req.params.clone(),
            timeout_ms: u32::try_from(opts.timeout.as_millis()).unwrap_or(u32::MAX),
        });
        match outbound.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                runtime_log::log_warn(format!("[browser_bridge] request {id}: peer {peer} is not taking messages (outbound queue full)"));
                return Err(BridgeError::Disconnected);
            }
            Err(TrySendError::Disconnected(_)) => return Err(BridgeError::Disconnected),
        }
        let outcome = wait_for_reply(id, &reply_rx, &budget, opts);
        let dropped = budget.dropped_progress.load(Ordering::Relaxed);
        if dropped > 0 {
            runtime_log::log_warn(format!("[browser_bridge] request {id}: {dropped} progress event(s) dropped (event queue full)"));
        }
        if let Err(err) = &outcome
            && matches!(err, BridgeError::Cancelled | BridgeError::Timeout | BridgeError::AttentionTimeout)
        {
            match outbound.try_send(AppMessage::Cancel(CancelMsg { id })) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => runtime_log::log_warn(format!("[browser_bridge] request {id}: outbound queue full; cancel dropped (the extension stops at its own deadline)")),
                Err(TrySendError::Disconnected(_)) => runtime_log::log_info(format!("[browser_bridge] request {id}: peer left before the cancel was queued")),
            }
        }
        outcome
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Removes a request's pending entry on every exit path of `request`.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.shared.pending).remove(&self.id);
    }
}

fn is_cancelled(opts: &RequestOptions<'_>) -> bool {
    opts.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
}

/// The requester's wait loop (see `BridgeServer::request`). Never holds a lock. Every received
/// event gives one unit of `budget.queued` back to the connection thread.
fn wait_for_reply(id: u64, replies: &mpsc::Receiver<Reply>, budget: &EventBudget, opts: &RequestOptions<'_>) -> Result<Value, BridgeError> {
    let mut deadline = Instant::now() + opts.timeout;
    let mut attention_since: Option<Instant> = None;
    loop {
        if is_cancelled(opts) {
            return Err(BridgeError::Cancelled);
        }
        let now = Instant::now();
        let remaining = if let Some(since) = attention_since {
            let limit = since + opts.attention_timeout;
            if now >= limit {
                runtime_log::log_warn(format!("[browser_bridge] request {id}: attention not resolved within {:?}", opts.attention_timeout));
                return Err(BridgeError::AttentionTimeout);
            }
            limit - now
        } else {
            if now >= deadline {
                runtime_log::log_warn(format!("[browser_bridge] request {id}: no response within {:?}", opts.timeout));
                return Err(BridgeError::Timeout);
            }
            deadline - now
        };
        match replies.recv_timeout(remaining.min(CANCEL_SLICE)) {
            Ok(Reply::Response(result)) => return result,
            Ok(Reply::Event(event)) => {
                budget.queued.fetch_sub(1, Ordering::AcqRel);
                match event.kind {
                    EventKind::Attention => {
                        attention_since.get_or_insert_with(Instant::now);
                    }
                    EventKind::AttentionCleared => {
                        // The deadline was suspended for the whole attention span.
                        if let Some(since) = attention_since.take() {
                            deadline += since.elapsed();
                        }
                    }
                    EventKind::Progress => {}
                }
                if let Some(on_event) = opts.on_event {
                    on_event(&event);
                }
            }
            Ok(Reply::Disconnected) | Err(RecvTimeoutError::Disconnected) => return Err(BridgeError::Disconnected),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Decrements the connection count when a connection thread ends (or never started).
struct ConnectionSlot(Arc<Shared>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The listener thread: accepts loopback connections until shutdown, one thread each.
fn listen_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    while !shared.is_shut_down() {
        match listener.accept() {
            Ok((stream, addr)) => {
                // Bound to 127.0.0.1, so this cannot happen; refuse anyway rather than trust it.
                if !addr.ip().is_loopback() {
                    runtime_log::log_warn(format!("[browser_bridge] refused non-loopback connection from {addr}"));
                    continue;
                }
                if shared.connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    runtime_log::log_warn(format!("[browser_bridge] refused connection: {MAX_CONNECTIONS} connections already open"));
                    continue;
                }
                // Accepted sockets inherit the listener's non-blocking mode on some platforms.
                if let Err(err) = stream.set_nonblocking(false) {
                    runtime_log::log_warn(format!("[browser_bridge] could not configure an accepted socket: {err}"));
                    continue;
                }
                shared.connections.fetch_add(1, Ordering::Relaxed);
                let slot = ConnectionSlot(Arc::clone(shared));
                let spawned = std::thread::Builder::new().name("ms-bridge-conn".into()).spawn(move || {
                    connection::serve(&slot.0, stream);
                    drop(slot);
                });
                if let Err(err) = spawned {
                    runtime_log::log_error(format!("[browser_bridge] could not spawn a connection thread: {err}"));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(shared.cfg.timings.poll),
            Err(err) => {
                runtime_log::log_warn(format!("[browser_bridge] accept failed: {err}"));
                std::thread::sleep(shared.cfg.timings.poll);
            }
        }
    }
    runtime_log::log_info(format!("[browser_bridge] listener on port {} stopped", shared.port));
}
