/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/route_inputs.rs
Off-GUI-thread refresh of the OCR / detector routing inputs (effective `AiRuntime` +
provider-scoped SIGILL load decision) that the Translation tab's per-frame route gates read.
Desktop-only (the native runtime is compiled out on the web build).

Main items:
- `OcrRouteInputs`: the `(AiRuntime, OrtLoadDecision)` pair the gates consume.
- `OcrRouteInputsRefresh`: the cached snapshot (unknown until the first read lands) plus the
  refresh state machine — at most one background read in flight, a new read started no more
  often than once per TTL, the result taken with a non-blocking `try_recv`.
- `gate_over_inputs`: evaluates a route gate against the snapshot, or — while it is still
  unknown — optimistically over the possible inputs, so no request is falsely refused.
- `spawn_route_inputs_read`: the production reader spawn (`ms_thread` worker running
  `crate::ocr::read_ocr_route_inputs`, which reads and parses `user_config` and may resolve the
  native selection — disk I/O and hardware probes, hence never on the GUI thread).

Notes:
- Until the first read lands the snapshot is `None`. A gate then passes when ANY possible input
  pair passes (`gate_over_inputs`): the local-AI availability (`--no-ai`, platform) still
  refuses for real, while a refusal that depends on the inputs (e.g. "backend unavailable" for a
  user whose runtime may be native) is not issued; the OCR / detector workers re-read the inputs
  themselves and route, or report the offline backend, with the real values.
- A failed spawn or a reader that died without answering is logged; the stale snapshot is kept
  and the next read is retried after the TTL, never in a tight loop.
- The reader is injected (`poll`'s `spawn` argument) so the state machine is tested without disk.
*/

use ms_config::{AiRuntime, OrtLoadDecision};
use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use web_time::{Duration, Instant};

/// The routing inputs the OCR and detector route gates consume: the effective AI runtime and
/// the SIGILL load decision for the provider the native runtime would load.
pub(super) type OcrRouteInputs = (AiRuntime, OrtLoadDecision);

/// Cached [`OcrRouteInputs`] refreshed by a background read (see the file header).
///
/// Never blocks: [`Self::snapshot`] returns the last landed value (`None` before the first), and
/// [`Self::poll`] only does a `try_recv` and, when the TTL elapsed and nothing is in flight,
/// starts one read through the injected spawner.
pub(super) struct OcrRouteInputsRefresh {
    /// Last landed snapshot; `None` until the first read lands.
    snapshot: Option<OcrRouteInputs>,
    /// When the last read was started (successfully spawned or not); `None` before the first.
    last_started_at: Option<Instant>,
    /// Receiver of the read in flight, if any. At most one at a time.
    in_flight: Option<Receiver<OcrRouteInputs>>,
}

impl OcrRouteInputsRefresh {
    /// A refresh with an unknown snapshot; the first [`Self::poll`] starts the first read.
    pub(super) fn new() -> Self {
        Self { snapshot: None, last_started_at: None, in_flight: None }
    }

    /// The current snapshot: the last landed read, or `None` before the first. No I/O.
    pub(super) fn snapshot(&self) -> Option<OcrRouteInputs> {
        self.snapshot
    }

    /// One non-blocking refresh step at `now`.
    ///
    /// Takes a finished read if one landed and returns it (`Some`) after adopting it as the
    /// snapshot; otherwise, when no read is in flight and at least `ttl` passed since the last
    /// start (or none was ever started), calls `spawn` with the sender the read must answer on.
    /// A `spawn` error, or a reader that dropped its sender without answering, is logged and
    /// keeps the stale snapshot; the next attempt waits for the TTL again.
    pub(super) fn poll<S>(&mut self, now: Instant, ttl: Duration, spawn: S) -> Option<OcrRouteInputs>
    where
        S: FnOnce(Sender<OcrRouteInputs>) -> std::io::Result<()>,
    {
        if let Some(rx) = &self.in_flight {
            match rx.try_recv() {
                Ok(inputs) => {
                    self.snapshot = Some(inputs);
                    self.in_flight = None;
                    return Some(inputs);
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = None;
                    ms_log::runtime_log::log_warn(format!(
                        "[ocr] route-inputs reader ended without a result; keeping the stale \
                         snapshot {:?} and retrying after {} ms",
                        self.snapshot,
                        ttl.as_millis()
                    ));
                }
            }
        }
        let due = self.last_started_at.is_none_or(|started| now.duration_since(started) >= ttl);
        if !due {
            return None;
        }
        self.last_started_at = Some(now);
        let (tx, rx) = mpsc::channel();
        match spawn(tx) {
            Ok(()) => self.in_flight = Some(rx),
            Err(err) => ms_log::runtime_log::log_error(format!(
                "[ocr] could not start the route-inputs reader thread: {err}; keeping the stale \
                 snapshot {:?} and retrying after {} ms",
                self.snapshot,
                ttl.as_millis()
            )),
        }
        None
    }
}

/// Production spawner for [`OcrRouteInputsRefresh::poll`]: a short-lived detached worker that
/// reads the inputs through `crate::ocr::read_ocr_route_inputs` (the same reader the OCR worker
/// routes with) and sends them on `tx`.
///
/// # Errors
/// The OS error when the thread could not be created.
pub(super) fn spawn_route_inputs_read(tx: Sender<OcrRouteInputs>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("ocr-route-inputs".to_owned())
        .spawn(move || {
            let inputs = crate::ocr::read_ocr_route_inputs();
            // A closed receiver means the tab state was dropped while the read ran: there is
            // nobody left to deliver the snapshot to, so the result is intentionally discarded.
            if tx.send(inputs).is_err() {
                ms_log::runtime_log::log_info("[ocr] route-inputs read finished after the tab closed; result dropped");
            }
        })
        // Detached on purpose: the read is short and owns nothing the tab must wait for.
        .map(|_detached| ())
}

/// Inputs a gate is tried with while the snapshot is unknown, in order: the native runtime
/// with a Safe guard (the only pair that can yield a native route), then the backend runtime
/// (which stands for every other pair: a Suspect guard routes exactly like the backend runtime).
const PENDING_CANDIDATES: [OcrRouteInputs; 2] =
    [(AiRuntime::Native, OrtLoadDecision::Safe), (AiRuntime::Backend, OrtLoadDecision::Safe)];

/// Evaluates a route `gate` (the owner's rule, e.g. `crate::ocr::ocr_route_availability`)
/// against `snapshot`; while the snapshot is unknown, returns the first candidate's `Ok` from
/// [`PENDING_CANDIDATES`], else the last candidate's error.
///
/// So before the first background read lands a request is refused only when it would be
/// refused under every possible input, and an `Ok(native route)` keeps a caller from adding a
/// "backend unavailable" refusal the real inputs may not warrant. No I/O.
///
/// # Errors
/// The gate's error for the known snapshot, or the last candidate's error when no candidate
/// passes.
pub(super) fn gate_over_inputs<T, E>(snapshot: Option<OcrRouteInputs>, gate: impl Fn(OcrRouteInputs) -> Result<T, E>) -> Result<T, E> {
    match snapshot {
        Some(inputs) => gate(inputs),
        None => gate(PENDING_CANDIDATES[0]).or_else(|_| gate(PENDING_CANDIDATES[1])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{OcrEngine, ocr_route_availability};
    use ms_local_ai::local_ai::{AiRouteKind, LocalAiAvailability};
    use ms_platform::{PlatformFeature, Unavailable, UnavailableReason};

    const TTL: Duration = Duration::from_millis(500);
    const NATIVE: OcrRouteInputs = (AiRuntime::Native, OrtLoadDecision::Safe);
    const SUSPECT: OcrRouteInputs = (AiRuntime::Native, OrtLoadDecision::Suspect);

    /// A spawner that must not be called.
    fn no_spawn(_tx: Sender<OcrRouteInputs>) -> std::io::Result<()> {
        panic!("spawn must not be called here");
    }

    /// A spawner whose reader answers `inputs` at once.
    fn answer(inputs: OcrRouteInputs) -> impl FnOnce(Sender<OcrRouteInputs>) -> std::io::Result<()> {
        move |tx| tx.send(inputs).map_err(|_| std::io::Error::other("receiver closed"))
    }

    /// A refresh whose first read (started at `t0`) has landed with `inputs`.
    fn landed(t0: Instant, inputs: OcrRouteInputs) -> OcrRouteInputsRefresh {
        let mut refresh = OcrRouteInputsRefresh::new();
        assert_eq!(refresh.poll(t0, TTL, answer(inputs)), None);
        assert_eq!(refresh.poll(t0, TTL, no_spawn), Some(inputs));
        refresh
    }

    #[test]
    fn unknown_until_first_read_lands() {
        let mut refresh = OcrRouteInputsRefresh::new();
        let t0 = Instant::now();
        let mut slot = None;
        assert_eq!(refresh.poll(t0, TTL, |tx| {
            slot = Some(tx);
            Ok(())
        }), None);
        assert_eq!(refresh.snapshot(), None);
        // In flight, nothing answered yet: no second spawn, still unknown.
        assert_eq!(refresh.poll(t0 + TTL * 3, TTL, no_spawn), None);
        assert_eq!(refresh.snapshot(), None);
        slot.expect("first poll spawns").send(NATIVE).expect("receiver alive");
        assert_eq!(refresh.poll(t0 + TTL * 3, TTL, no_spawn), Some(NATIVE));
        assert_eq!(refresh.snapshot(), Some(NATIVE));
    }

    #[test]
    fn next_read_waits_for_ttl_since_last_start() {
        let t0 = Instant::now();
        let mut refresh = landed(t0, NATIVE);
        // Within the TTL of the last start: no new read.
        assert_eq!(refresh.poll(t0 + TTL / 2, TTL, no_spawn), None);
        assert_eq!(refresh.poll(t0 + TTL, TTL, answer(SUSPECT)), None);
        assert_eq!(refresh.poll(t0 + TTL, TTL, no_spawn), Some(SUSPECT));
        assert_eq!(refresh.snapshot(), Some(SUSPECT));
    }

    #[test]
    fn spawn_failure_keeps_stale_snapshot_and_retries_after_ttl() {
        let t0 = Instant::now();
        let mut refresh = landed(t0, NATIVE);
        assert_eq!(refresh.poll(t0 + TTL, TTL, |_tx| Err(std::io::Error::other("no threads"))), None);
        assert_eq!(refresh.snapshot(), Some(NATIVE));
        // No tight retry loop after a failure.
        assert_eq!(refresh.poll(t0 + TTL * 3 / 2, TTL, no_spawn), None);
        let mut spawned = false;
        refresh.poll(t0 + TTL * 2, TTL, |_tx| {
            spawned = true;
            Ok(())
        });
        assert!(spawned);
    }

    #[test]
    fn dead_reader_keeps_stale_snapshot_and_allows_retry() {
        let t0 = Instant::now();
        let mut refresh = landed(t0, NATIVE);
        // The reader drops its sender without answering.
        refresh.poll(t0 + TTL, TTL, |tx| {
            drop(tx);
            Ok(())
        });
        assert_eq!(refresh.poll(t0 + TTL * 3 / 2, TTL, no_spawn), None);
        assert_eq!(refresh.snapshot(), Some(NATIVE));
        let mut spawned = false;
        refresh.poll(t0 + TTL * 2, TTL, |_tx| {
            spawned = true;
            Ok(())
        });
        assert!(spawned, "a dead reader frees the slot for the next read");
    }

    #[test]
    fn known_snapshot_is_passed_through() {
        let seen = gate_over_inputs(Some(SUSPECT), Err::<(), _>);
        assert_eq!(seen, Err(SUSPECT));
    }

    fn platform(on: bool, feature: PlatformFeature) -> Result<(), Unavailable> {
        if on { Ok(()) } else { Err(Unavailable::new(feature, UnavailableReason::NotOnThisPlatform)) }
    }

    /// Before the first read lands, the OCR gate refuses only what every possible input pair
    /// refuses, and reports a backend route (which the caller then health-gates) only when no
    /// input pair could route natively — so no false "backend unavailable" refusal.
    #[test]
    fn pending_ocr_gate_never_refuses_falsely() {
        const ALL_INPUTS: [OcrRouteInputs; 4] = [
            (AiRuntime::Backend, OrtLoadDecision::Safe),
            (AiRuntime::Backend, OrtLoadDecision::Suspect),
            (AiRuntime::Native, OrtLoadDecision::Safe),
            (AiRuntime::Native, OrtLoadDecision::Suspect),
        ];
        const ENGINES: [OcrEngine; 7] = [
            OcrEngine::MangaOcr,
            OcrEngine::EasyOcr,
            OcrEngine::PaddleOcr,
            OcrEngine::PaddleVl,
            OcrEngine::Surya,
            OcrEngine::AiApi,
            OcrEngine::Baberu,
        ];
        for no_ai in [false, true] {
            for backend_platform in [false, true] {
                for native_platform in [false, true] {
                    let local = LocalAiAvailability::resolve(
                        no_ai,
                        platform(backend_platform, PlatformFeature::PythonBackend),
                        platform(native_platform, PlatformFeature::NativeOnnxRuntime),
                        // OCR never runs on sd.cpp, so its platform answer is irrelevant here.
                        Ok(()),
                    );
                    for engine in ENGINES {
                        for model in ["base_onnx", "base_torch"] {
                            let case = format!("no_ai={no_ai} backend={backend_platform} native={native_platform} {engine:?} {model}");
                            let gate = |(runtime, guard): OcrRouteInputs| ocr_route_availability(engine, model, runtime, guard, &local);
                            let pending = gate_over_inputs(None, gate);
                            let any_ok = ALL_INPUTS.iter().any(|&inputs| gate(inputs).is_ok());
                            assert_eq!(pending.is_ok(), any_ok, "{case}");
                            if pending == Ok(AiRouteKind::Backend) {
                                assert!(
                                    ALL_INPUTS.iter().all(|&inputs| gate(inputs) != Ok(AiRouteKind::Native)),
                                    "{case}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
