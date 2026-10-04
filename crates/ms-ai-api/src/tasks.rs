/*
File: crates/ms-ai-api/src/tasks.rs

Purpose:
Background execution of the connection widget's blocking operations (store key, delete key,
refresh metadata) so they never run on the GUI thread.

Key structures:
- AiApiRequest    : one blocking operation; its `Debug` redacts the key.
- AiApiEvent      : the result of one request, delivered back to the GUI thread.
- AiApiTaskRunner : feeds requests to one lazily spawned serial `ms_thread` worker and collects
                    the events.
- AiApiPollSummary: what `poll_and_apply` changed (model replaced, notices to show).

Key functions:
- AiApiTaskRunner::submit / poll
- AiApiTaskRunner::submit_actions / poll_and_apply (the connection-state convenience pair)

Notes:
Each runner executes its requests one at a time in submission order on its own detached worker
(spawned on the first `submit`, fed by a channel, exiting when the runner is dropped), so a
refresh can never overwrite a newer one and a save followed by a delete reaches the credential
store in that order. Runners are independent of each other and of the OCR / MT run workers.
`AiApiConnectionState::apply_event` drops results for a service that is no longer selected. A
failed thread spawn is logged and reported as an immediate `Failed` event, never a panic; a dead
worker is logged and replaced on the next `submit`. Requests and events are `Send`; the runner
itself lives on the GUI thread.
*/

use std::cell::RefCell;
use std::fmt;
use std::sync::mpsc::{Receiver, SendError, Sender, channel};

use ms_log::runtime_log;

use crate::connection::{AiApiConnectionActions, AiApiConnectionState, AiApiNotice};
use crate::error::AiApiError;
use crate::metadata::AiApiMetadata;
use crate::service::AiApiService;

/// One blocking connection operation for a worker thread.
pub enum AiApiRequest {
    /// Store `key` (trimmed by `keys::store_api_key`) as the API key of `service`.
    StoreKey { service: AiApiService, key: String },
    /// Delete the stored API key of `service`.
    ClearKey { service: AiApiService },
    /// Reload key state, model list and account status of `service`.
    RefreshMetadata { service: AiApiService },
}

impl AiApiRequest {
    /// The service this request acts on.
    #[must_use]
    pub fn service(&self) -> AiApiService {
        match self {
            Self::StoreKey { service, .. } | Self::ClearKey { service } | Self::RefreshMetadata { service } => *service,
        }
    }

    /// Short request kind for log lines (diagnostics only; never contains the key).
    fn kind(&self) -> &'static str {
        match self {
            Self::StoreKey { .. } => "store-key",
            Self::ClearKey { .. } => "clear-key",
            Self::RefreshMetadata { .. } => "refresh-metadata",
        }
    }

    /// Runs the request on the calling thread (blocking credential-store / network I/O) and
    /// turns the outcome into the event the GUI applies. Worker threads only.
    fn execute(self) -> AiApiEvent {
        match self {
            Self::StoreKey { service, key } => match crate::keys::store_api_key(service, &key) {
                Ok(()) => AiApiEvent::KeyStored { service },
                Err(error) => AiApiEvent::Failed { service, error },
            },
            Self::ClearKey { service } => match crate::keys::clear_api_key(service) {
                Ok(()) => AiApiEvent::KeyCleared { service },
                Err(error) => AiApiEvent::Failed { service, error },
            },
            Self::RefreshMetadata { service } => match crate::metadata::load_metadata(service) {
                Ok(metadata) => AiApiEvent::MetadataLoaded(metadata),
                Err(error) => AiApiEvent::Failed { service, error },
            },
        }
    }
}

// Manual `Debug`: the plaintext key must never reach a log or a panic message.
impl fmt::Debug for AiApiRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StoreKey { service, .. } => f.debug_struct("StoreKey").field("service", service).field("key", &"<redacted>").finish(),
            Self::ClearKey { service } => f.debug_struct("ClearKey").field("service", service).finish(),
            Self::RefreshMetadata { service } => f.debug_struct("RefreshMetadata").field("service", service).finish(),
        }
    }
}

/// Result of one `AiApiRequest`. Store, delete and refresh failures share `Failed`; the
/// error's `Display` is the localized message.
#[derive(Debug)]
pub enum AiApiEvent {
    KeyStored { service: AiApiService },
    KeyCleared { service: AiApiService },
    MetadataLoaded(AiApiMetadata),
    Failed { service: AiApiService, error: AiApiError },
}

/// What `AiApiTaskRunner::poll_and_apply` changed in the connection state.
#[derive(Debug, Default)]
pub struct AiApiPollSummary {
    /// The selected model was replaced (persisted field changed: the consumer saves settings).
    pub model_changed: bool,
    /// Toasts to show, in event order.
    pub notices: Vec<AiApiNotice>,
}

/// Name of every runner's serial worker thread (diagnostics only).
const WORKER_THREAD_NAME: &str = "ai-api-requests";

/// Runs `AiApiRequest`s on one background worker, strictly in submission order, and hands their
/// events back to the GUI thread in that same order. One runner per connection widget instance;
/// it is owned and polled by the GUI thread (`!Sync`: the queue slot is a `RefCell`).
#[derive(Debug)]
pub struct AiApiTaskRunner {
    events_tx: Sender<AiApiEvent>,
    events_rx: Receiver<AiApiEvent>,
    /// Request queue of the serial worker: `None` until the first `submit` spawns it, or after a
    /// spawn failure. Dropping it (with the runner) lets the worker drain what is queued and exit.
    queue: RefCell<Option<Sender<AiApiRequest>>>,
    /// Runs one request on the worker thread: `AiApiRequest::execute`, except in unit tests.
    executor: fn(AiApiRequest) -> AiApiEvent,
}

impl Default for AiApiTaskRunner {
    fn default() -> Self {
        Self::with_executor(AiApiRequest::execute)
    }
}

impl AiApiTaskRunner {
    /// A runner whose worker turns each request into an event with `executor`.
    fn with_executor(executor: fn(AiApiRequest) -> AiApiEvent) -> Self {
        let (events_tx, events_rx) = channel();
        Self { events_tx, events_rx, queue: RefCell::new(None), executor }
    }

    /// Queues `request` behind every earlier one and returns immediately. The first call spawns
    /// the runner's serial worker; a worker that died (a panicking request unwinds it, losing
    /// what was queued on it) is logged and replaced, so this request still runs. If no worker
    /// can be spawned, the failure is logged and a `Failed { error: TaskSpawn }` event is queued
    /// for the next `poll`.
    pub fn submit(&self, request: AiApiRequest) {
        let mut queue = self.queue.borrow_mut();
        let request = match queue.as_ref() {
            Some(tx) => match tx.send(request) {
                Ok(()) => return,
                Err(SendError(request)) => {
                    runtime_log::log_error(format!("[AI API] the request worker stopped unexpectedly; requests still queued on it were lost; starting a new worker for {} of {}", request.kind(), request.service().label()));
                    *queue = None;
                    request
                }
            },
            None => request,
        };
        let service = request.service();
        let kind = request.kind();
        let events_tx = self.events_tx.clone();
        let executor = self.executor;
        let (queue_tx, queue_rx) = channel();
        // The first request is handed to the worker directly, so a spawn failure never leaves it
        // in a queue nobody reads. The `JoinHandle` is dropped (detached): the GUI thread never
        // joins; the worker exits on its own once the runner drops `queue_tx`.
        match ms_thread::Builder::new().name(WORKER_THREAD_NAME.to_string()).spawn(move || run_worker(request, &queue_rx, &events_tx, executor)) {
            Ok(_detached) => *queue = Some(queue_tx),
            Err(err) => {
                runtime_log::log_error(format!("[AI API] failed to spawn the {WORKER_THREAD_NAME} thread for {kind} of {}: {err}", service.label()));
                let event = AiApiEvent::Failed { service, error: AiApiError::TaskSpawn { detail: err.to_string() } };
                // `self` owns the receiver, so sending on its own channel cannot fail here; the
                // branch only keeps the result checked.
                if self.events_tx.send(event).is_err() {
                    runtime_log::log_error(format!("[AI API] could not queue the spawn failure for {}", service.label()));
                }
            }
        }
    }

    /// Drains every finished event without blocking, in submission order.
    #[must_use]
    pub fn poll(&self) -> Vec<AiApiEvent> {
        let mut events = Vec::new();
        // Stops at `Empty`. `Disconnected` cannot occur: `self.events_tx` keeps the channel connected
        // for the runner's whole life.
        while let Ok(event) = self.events_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// `state.begin_requests(actions)` followed by `submit` of each request, in order.
    pub fn submit_actions(&self, state: &mut AiApiConnectionState, actions: AiApiConnectionActions) {
        for request in state.begin_requests(actions) {
            self.submit(request);
        }
    }

    /// Polls, applies each event to `state`, submits every follow-up request (the metadata
    /// refresh after a stored key) and returns what the consumer must react to.
    pub fn poll_and_apply(&self, state: &mut AiApiConnectionState) -> AiApiPollSummary {
        let mut summary = AiApiPollSummary::default();
        for event in self.poll() {
            let outcome = state.apply_event(event);
            summary.model_changed |= outcome.model_changed;
            if let Some(request) = outcome.follow_up {
                self.submit(request);
            }
            if let Some(notice) = outcome.notice {
                summary.notices.push(notice);
            }
        }
        summary
    }
}

/// Body of a runner's serial worker: runs `first`, then every queued request in order, and
/// sends each event back. Ends when the runner drops the queue sender. Requests still queued
/// after the runner is gone keep running (a queued key save still lands); only their events are
/// discarded, with a warning.
fn run_worker(first: AiApiRequest, queue: &Receiver<AiApiRequest>, events_tx: &Sender<AiApiEvent>, executor: fn(AiApiRequest) -> AiApiEvent) {
    for request in std::iter::once(first).chain(queue.iter()) {
        let service = request.service();
        let kind = request.kind();
        // The receiver lives in the runner; it is gone only when the owning tab was dropped, so
        // the result has no one left to show it to.
        if events_tx.send(executor(request)).is_err() {
            runtime_log::log_warn(format!("[AI API] {kind} for {} finished after its runner was dropped; result discarded", service.label()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{AiApiEvent, AiApiRequest, AiApiTaskRunner};
    use crate::error::AiApiError;
    use crate::service::AiApiService;

    /// Test executor: the FIRST service submitted in `fifo_order_*` is the slowest, so a
    /// thread-per-request runner would deliver it last. No I/O, no credential store.
    #[expect(clippy::needless_pass_by_value, reason = "signature fixed by the runner's by-value executor fn-pointer type")]
    fn delayed_echo(request: AiApiRequest) -> AiApiEvent {
        let service = request.service();
        let delay_ms = match service {
            AiApiService::Groq => 120,
            AiApiService::OpenAi => 40,
            _ => 0,
        };
        std::thread::sleep(Duration::from_millis(delay_ms));
        AiApiEvent::Failed { service, error: AiApiError::EmptyKey }
    }

    /// Test executor that unwinds its worker on `ClearKey` and echoes everything else.
    #[expect(clippy::needless_pass_by_value, reason = "signature fixed by the runner's by-value executor fn-pointer type")]
    fn panics_on_clear(request: AiApiRequest) -> AiApiEvent {
        if let AiApiRequest::ClearKey { .. } = request {
            panic!("test executor: simulated worker crash");
        }
        AiApiEvent::Failed { service: request.service(), error: AiApiError::EmptyKey }
    }

    /// Services of `events`, in order (every test executor answers with `Failed`).
    fn event_services(events: &[AiApiEvent]) -> Vec<AiApiService> {
        events.iter().map(|event| match event {
            AiApiEvent::Failed { service, .. } | AiApiEvent::KeyStored { service } | AiApiEvent::KeyCleared { service } => *service,
            AiApiEvent::MetadataLoaded(metadata) => metadata.service,
        }).collect()
    }

    /// Polls until `count` events arrived or a 10 s deadline passed.
    fn poll_until(runner: &AiApiTaskRunner, count: usize) -> Vec<AiApiEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while events.len() < count && Instant::now() < deadline {
            events.extend(runner.poll());
            std::thread::sleep(Duration::from_millis(5));
        }
        events
    }

    #[test]
    fn fifo_order_of_completion_matches_submission() {
        let runner = AiApiTaskRunner::with_executor(delayed_echo);
        let submitted = [AiApiService::Groq, AiApiService::OpenAi, AiApiService::Gemini, AiApiService::Xai];
        for service in submitted {
            runner.submit(AiApiRequest::RefreshMetadata { service });
        }
        assert_eq!(event_services(&poll_until(&runner, submitted.len())), submitted);
    }

    #[test]
    fn dead_worker_is_replaced_on_next_submit() {
        let runner = AiApiTaskRunner::with_executor(panics_on_clear);
        runner.submit(AiApiRequest::ClearKey { service: AiApiService::Groq });
        // Probes sent before the worker has unwound are lost with it; the first probe after its
        // death must respawn a worker and come back.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while events.is_empty() && Instant::now() < deadline {
            runner.submit(AiApiRequest::RefreshMetadata { service: AiApiService::Gemini });
            std::thread::sleep(Duration::from_millis(20));
            events.extend(runner.poll());
        }
        assert_eq!(event_services(&events).first(), Some(&AiApiService::Gemini));
    }

    #[test]
    fn fresh_runner_polls_empty() {
        assert!(AiApiTaskRunner::default().poll().is_empty());
    }

    #[test]
    fn store_key_debug_redacts_the_key() {
        let request = AiApiRequest::StoreKey { service: AiApiService::Groq, key: "sk-secret-123".to_string() };
        let debug = format!("{request:?}");
        assert!(!debug.contains("sk-secret-123"), "{debug}");
        assert!(debug.contains("Groq"), "{debug}");
        assert_eq!(request.service(), AiApiService::Groq);
    }
}
