/*
File: cleaning/tools/ai_editor/engines/shared/stream_progress.rs

Purpose:
The progress state a large-model engine shares between its streaming workers and its panel —
a run, a model download, any other long backend operation — together with the generation
counter that keeps a retired job from writing into the next one's bar, the transfer-rate
estimator, the streamed frame parser and the streaming call that feeds them. Model-agnostic:
FLUX.2 klein and FLUX.3 klein both use it; a frame may carry an optional stable `stage` id
(FLUX.3) that the engine localizes.

Main responsibilities:
- own `EngineProgress` and the `Mutex` discipline around it (`lock_progress`,
  `begin_progress_generation`, `retire_progress_generation`, `update_progress`,
  `publish_progress_frame`);
- keep a bounded sliding window of byte samples — a download's overall counter, or the
  per-file counter of a load reading a model file from disk — and derive speed and ETA from
  it (`RateEstimator`, `RateSource`, `transfer_status`);
- parse a streamed progress frame (`parse_progress_frame`) and what a large-model run's
  answer says about a degraded run (`parse_degraded`);
- run one streaming backend call whose id a cancel can reach (`stream_call`), and ask the
  backend to stop one (`spawn_backend_cancel`) through the STICKY per-operation handshake
  (`CallControl` in the `ProgressClaim`: the id and the cancel meet in either order, one backend
  cancel goes out);
- keep a cancelled operation counted as busy until its worker ends (`AbandonedJobs`);
- format the rate and duration strings the bar shows (bytes: `large_models::format_gib`).

Key structures:
- `EngineProgress`, `ProgressClaim`, `CallControl`, `AbandonedJobs`, `RateEstimator`, `RateSource`,
  `FileProgress`, `ProgressFrame`, `RunDegraded`

Key functions:
- `begin_progress_generation()`, `retire_progress_generation()`, `update_progress()`,
  `publish_progress_frame()`
- `progress_fraction()`, `progress_fractions()`, `transfer_status()`
- `parse_progress_frame()`, `parse_degraded()`, `stream_call()`, `map_call_error()`,
  `spawn_backend_cancel()`
- `format_mib_per_second()`, `format_eta()`, `file_component_label()`

Notes:
The GENERATION is the invariant: a frame whose generation no longer matches the live one
is dropped, so a cancelled job can never move the bar of the job that replaced it. The
DRAWING of the bar (`draw_stream_progress`) lives in `stream_progress_ui.rs`. Every text is a
`cleaning.tools.engine_progress.*` key; a model's own wording stays in its engine.
*/

use ms_backend_ipc::{self as backend_ipc, CallError};
use ms_local_ai::backend_health::ai_backend_offline_error;
use ms_local_ai::large_models::{FailureKind, LargeModelStep, RamStep, VramStep};
use ms_thread as thread;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use web_time::{Duration, Instant};

/// Live progress shared between an engine's workers and its own panel.
///
/// ONE instance is owned by the engine and reused by every operation, so it is claimed by
/// GENERATION: [`begin_progress_generation`] hands the next number to a starting operation
/// and every later write from an older one — including its terminal `active = false` —
/// is dropped by [`update_progress`]. Without that, a cancelled worker finishing a
/// minute later would erase the bar of the operation that replaced it and leave a bare
/// spinner until that one ended.
#[derive(Default)]
pub(in crate::tools::ai_editor::engines) struct EngineProgress {
    /// The operation that currently owns every other field. `0` = nothing has started
    /// yet, which no worker can ever carry.
    pub(in crate::tools::ai_editor::engines) generation: u64,
    pub(in crate::tools::ai_editor::engines) active: bool,
    pub(in crate::tools::ai_editor::engines) phase: String,
    pub(in crate::tools::ai_editor::engines) step: u64,
    pub(in crate::tools::ai_editor::engines) total: u64,
    pub(in crate::tools::ai_editor::engines) label: String,
    /// The STABLE id of the backend stage this frame describes (FLUX.3 klein's `stage`), which
    /// the engine localizes; `None` for a backend that sends none (FLUX.2), whose `label` is
    /// then the only text there is.
    pub(in crate::tools::ai_editor::engines) stage: Option<String>,
    /// The file a model download is transferring or a load is reading right now, `None` for
    /// every operation and every phase that has no second level. See [`FileProgress`].
    pub(in crate::tools::ai_editor::engines) file: Option<FileProgress>,
    /// Transfer rate of the counter `rate_source` names: the OVERALL counter of `download`
    /// frames, or the per-file counter of `load` frames that read a file from disk. Reset by
    /// [`begin_progress_generation`] and whenever the source changes, so a new operation (or
    /// the next file) never inherits the previous transfer's speed.
    pub(in crate::tools::ai_editor::engines) rate: RateEstimator,
    /// What `rate` measures; `None` until a frame with a byte counter arrived.
    pub(in crate::tools::ai_editor::engines) rate_source: Option<RateSource>,
    /// The cancellation control of the operation that owns `generation` (`None` before the
    /// first claim). Deliberately NOT a display field written through [`update_progress`]:
    /// the worker holds its own clone through [`ProgressClaim`], so an id it publishes AFTER a
    /// cancel still reaches the control and is cancelled at once ([`CallControl`]).
    pub(in crate::tools::ai_editor::engines) control: Option<Arc<CallControl>>,
    /// The function a control sends a backend cancel with; `None` = [`spawn_backend_cancel`].
    /// A test injects a recording one; the product never sets it.
    pub(in crate::tools::ai_editor::engines) canceller: Option<Canceller>,
}

/// Sends the backend cancel of request `id`; `log_label` names the engine in the log.
pub(in crate::tools::ai_editor::engines) type Canceller = Arc<dyn Fn(u64, &'static str) + Send + Sync>;

/// The STICKY cancellation handshake of one operation: the request id the worker publishes and
/// the cancel the GUI requests, in either order, meet here and send EXACTLY ONE backend cancel.
///
/// It exists because the id is known only once `begin_call` returned on the worker, while the
/// user may cancel before that: a cancel that found no id used to be lost, and the id published
/// afterwards went into a retired display generation, so the backend computed a run nobody
/// waited for. Now whichever side comes second sends the cancel.
pub(in crate::tools::ai_editor::engines) struct CallControl {
    state: Mutex<CallControlState>,
    canceller: Canceller,
}

/// The mutable half of a [`CallControl`].
#[derive(Debug, Default)]
struct CallControlState {
    /// The IPC id, once the request is on the wire.
    request_id: Option<u64>,
    /// The log label of a requested cancel; `Some` = cancel requested (sticky).
    cancel_requested: Option<&'static str>,
    /// The one backend cancel was sent.
    cancel_sent: bool,
}

impl CallControl {
    /// A control with nothing published and nothing requested.
    pub(in crate::tools::ai_editor::engines) fn new(canceller: Canceller) -> Self {
        Self { state: Mutex::new(CallControlState::default()), canceller }
    }

    fn lock(&self) -> MutexGuard<'_, CallControlState> {
        // Recovered from poisoning: the state is three plain fields, each write complete.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Worker side: the request is on the wire as `id`. When a cancel was already requested,
    /// the backend cancel is sent now. The canceller runs outside the lock.
    pub(in crate::tools::ai_editor::engines) fn publish_request_id(&self, id: u64) {
        let send = {
            let mut state = self.lock();
            state.request_id = Some(id);
            Self::take_due_cancel(&mut state)
        };
        if let Some((id, label)) = send {
            (self.canceller)(id, label);
        }
    }

    /// GUI side: stop this operation. Sends the backend cancel now when the id is known, else
    /// leaves the request standing for [`Self::publish_request_id`]. Idempotent.
    pub(in crate::tools::ai_editor::engines) fn cancel(&self, log_label: &'static str) {
        let send = {
            let mut state = self.lock();
            if state.cancel_requested.is_none() {
                state.cancel_requested = Some(log_label);
            }
            Self::take_due_cancel(&mut state)
        };
        if let Some((id, label)) = send {
            (self.canceller)(id, label);
        }
    }

    /// Whether a cancel was requested (tests observe the handshake through it).
    #[cfg(test)]
    pub(in crate::tools::ai_editor::engines) fn cancel_requested(&self) -> bool {
        self.lock().cancel_requested.is_some()
    }

    /// The `(id, label)` to cancel when both halves are present and nothing was sent yet;
    /// marks it sent.
    fn take_due_cancel(state: &mut CallControlState) -> Option<(u64, &'static str)> {
        if state.cancel_sent {
            return None;
        }
        let pair = state.request_id.zip(state.cancel_requested)?;
        state.cancel_sent = true;
        Some(pair)
    }
}

/// What a claim of the shared progress hands the operation's worker: the generation every
/// display write carries, and the operation's own [`CallControl`] (published ids go there).
#[derive(Clone)]
pub(in crate::tools::ai_editor::engines) struct ProgressClaim {
    pub(in crate::tools::ai_editor::engines) generation: u64,
    pub(in crate::tools::ai_editor::engines) control: Arc<CallControl>,
}

impl ProgressClaim {
    /// The `on_started` of [`stream_call`]: publish the request id to this operation's control.
    pub(in crate::tools::ai_editor::engines) fn publish_request_id(&self, id: u64) {
        self.control.publish_request_id(id);
    }
}

/// A receiver whose answer nobody wants any more but whose work still occupies the backend.
trait Settles: Send {
    /// `true` once the worker has answered or is gone.
    fn settled(&self) -> bool;
}

impl<T: Send> Settles for Receiver<T> {
    fn settled(&self) -> bool {
        !matches!(self.try_recv(), Err(TryRecvError::Empty))
    }
}

/// The CANCELLED operations whose workers have not ended yet.
///
/// A cancel retires the operation for the UI at once, but the backend stops it only at its next
/// check (a stage, a denoise step, a download chunk); starting the next operation meanwhile
/// would queue it behind the first and could stack several expensive requests by quick
/// start/cancel cycles. An engine therefore keeps the abandoned receiver here and refuses a new
/// operation while [`Self::busy`] (the `AiEngine::cancel` contract allows exactly this); the
/// answer, when it lands, is dropped unread.
#[derive(Default)]
pub(in crate::tools::ai_editor::engines) struct AbandonedJobs {
    jobs: Vec<Box<dyn Settles>>,
}

impl AbandonedJobs {
    /// Keeps `rx` until its worker ends.
    pub(in crate::tools::ai_editor::engines) fn push<T: Send + 'static>(&mut self, rx: Receiver<T>) {
        self.jobs.push(Box::new(rx));
    }

    /// Drops every settled job; returns whether one is still running.
    pub(in crate::tools::ai_editor::engines) fn poll(&mut self) -> bool {
        self.jobs.retain(|job| !job.settled());
        !self.jobs.is_empty()
    }

    /// Whether a cancelled job is still running (as of the last [`Self::poll`]).
    pub(in crate::tools::ai_editor::engines) fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }
}

/// Wall-clock span the transfer-rate estimator averages over.
///
/// A TIME window rather than a fixed number of frames: the backend throttles progress to
/// roughly ten frames per second AND at least 4 MB apart, so the frame CADENCE varies with
/// the line speed — a fast link reports every 4 MB, a slow one ten times a second. Averaging
/// over N frames would therefore average over a different amount of wall time at every
/// speed, while this averages over a known five seconds whatever the cadence.
pub(in crate::tools::ai_editor::engines) const RATE_WINDOW: Duration = Duration::from_secs(5);

/// Shortest span the window must cover before a rate is reported at all.
///
/// Two consecutive frames can be 100 ms apart and 4 MB wide, which is a 40 MB/s reading
/// taken from a single burst; on a transfer measured in hours an estimate built from that
/// is worse than no estimate. Nothing is shown until the window spans this much real time.
pub(in crate::tools::ai_editor::engines) const RATE_MIN_SPAN: Duration = Duration::from_millis(1500);

/// Wall-clock span the DISK-READ rate (a model load's per-file counter) averages over.
///
/// Shorter than [`RATE_WINDOW`]: a load reads a component in seconds to a minute, so a
/// five-second average would trail the real speed for most of it, while one second at the
/// backend's ~5 frames per second still averages several chunks and does not jump.
pub(in crate::tools::ai_editor::engines) const DISK_READ_RATE_WINDOW: Duration = Duration::from_secs(1);

/// Shortest span the disk-read window must cover before a rate is shown (see
/// [`RATE_MIN_SPAN`]): one full window, so the first number is already a one-second average.
pub(in crate::tools::ai_editor::engines) const DISK_READ_RATE_MIN_SPAN: Duration = Duration::from_secs(1);

/// The byte counter a [`RateEstimator`] measures.
///
/// A download is measured over its OVERALL counter (`step`); any other frame that carries a
/// file level (a load, the encode phase, a denoise re-reading disk-tier blocks) over the
/// counter of the ONE file it is reading (`file_step`), because its `step` is a component or
/// step index, not bytes. The per-file counter restarts at every file, so a file read is keyed by the
/// file's component and label and a new key starts a new window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum RateSource {
    /// `download` frames: the overall byte counter across the whole plan.
    Download,
    /// Non-download frames with a file level (a load, the encode phase, a denoise re-read):
    /// the bytes read of this one file.
    FileRead {
        /// The frame's `file_component` (`None` = the legacy transformer-only frames).
        component: Option<String>,
        /// The frame's `file_label`.
        label: String,
    },
}

impl RateSource {
    /// The counter `frame` contributes to a rate, or `None` for a frame without bytes
    /// (a generation's steps without a file level, a load step between files).
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn of(frame: &ProgressFrame) -> Option<(Self, u64)> {
        match (frame.phase.as_str(), frame.file.as_ref()) {
            ("download", _) => Some((Self::Download, frame.step)),
            // Any other phase with a file level is reading a model file from disk: a load step,
            // the encode phase reading the encoder, or a denoise re-reading disk-tier blocks.
            (_, Some(file)) => Some((Self::FileRead { component: file.component.clone(), label: file.label.clone() }, file.step)),
            (_, None) => None,
        }
    }

    /// A fresh estimator with this source's window.
    fn estimator(&self) -> RateEstimator {
        match self {
            Self::Download => RateEstimator::default(),
            Self::FileRead { .. } => RateEstimator::with_window(DISK_READ_RATE_WINDOW, DISK_READ_RATE_MIN_SPAN),
        }
    }
}

/// Rolling transfer-rate estimator over ONE monotonically growing byte counter.
///
/// A download feeds the OVERALL counter and never its per-file counters: the per-file counter
/// resets at every file boundary, which on a 35 GB plan would make the rate jump to nonsense
/// several times per gigabyte. A load feeds the per-file counter of the file it reads, with a
/// new estimator per file ([`RateSource`]).
///
/// The rate is the total byte delta across the retained window divided by the wall time it
/// spans — a moving average over `window` ([`RATE_WINDOW`] for a download,
/// [`DISK_READ_RATE_WINDOW`] for a load). An exponential average was the alternative and was
/// rejected: its smoothing factor only means something relative to a fixed sample interval,
/// and this stream's interval is set by whichever of the backend's two throttles binds, which
/// changes with the line speed.
#[derive(Debug, Clone)]
pub(in crate::tools::ai_editor::engines) struct RateEstimator {
    /// `(observed_at, counter)`, oldest first. Bounded by the window, so at ten frames
    /// per second it holds about fifty entries.
    pub(in crate::tools::ai_editor::engines) samples: VecDeque<(Instant, u64)>,
    /// The span averaged over.
    window: Duration,
    /// The shortest span that yields a rate at all.
    min_span: Duration,
}

impl Default for RateEstimator {
    /// The download estimator: [`RATE_WINDOW`] and [`RATE_MIN_SPAN`].
    fn default() -> Self {
        Self::with_window(RATE_WINDOW, RATE_MIN_SPAN)
    }
}

impl RateEstimator {
    /// An empty estimator averaging over `window`, silent until `min_span` is covered.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn with_window(window: Duration, min_span: Duration) -> Self {
        Self { samples: VecDeque::new(), window, min_span }
    }

    /// Records one overall-byte observation.
    ///
    /// A BACKWARDS step discards the whole window and starts a new one from this sample.
    /// That is not defensive coding for an impossible case: a resumed file whose final
    /// length is wrong is refetched from zero exactly once, so the overall counter really
    /// does move backwards once per such file (`ipc/PROTOCOL.md`, `download.start`). The
    /// samples before the restart describe bytes that are being sent again, so averaging
    /// across the seam would report a rate that never happened; dropping them costs a few
    /// seconds of "no estimate" and is the only honest answer.
    pub(in crate::tools::ai_editor::engines) fn observe(&mut self, now: Instant, step: u64) {
        if self.samples.back().is_some_and(|&(_, last)| step < last) {
            self.samples.clear();
        }
        self.samples.push_back((now, step));
        // Keep one sample older than the window so the span is never shorter than it.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|&(t, _)| now.duration_since(t) > self.window)
        {
            self.samples.pop_front();
        }
    }

    /// Bytes per second averaged over the retained window, or `None` while there is not
    /// enough history for the number to mean anything.
    ///
    /// `None` covers every degenerate case rather than reporting a number: fewer than two
    /// samples, a window shorter than its minimum span ([`RATE_MIN_SPAN`] for a download), and
    /// a window in which the
    /// byte counter did not move.
    pub(in crate::tools::ai_editor::engines) fn bytes_per_second(&self) -> Option<f64> {
        let (first_at, first_step) = *self.samples.front()?;
        let (last_at, last_step) = *self.samples.back()?;
        let span = last_at.duration_since(first_at);
        if self.samples.len() < 2 || span < self.min_span {
            return None;
        }
        // The backwards case is already handled by `observe`, so this cannot underflow; the
        // checked form keeps that true if the ordering rule is ever changed.
        let moved = last_step.checked_sub(first_step)?;
        if moved == 0 {
            return None;
        }
        // Cast justification: a byte delta of at most tens of gigabytes and a span of a few
        // seconds are both far inside f64's exact-integer range, and the result is a rate
        // rendered to one decimal.
        Some(moved as f64 / span.as_secs_f64())
    }

    /// Remaining wall time at the current rate, or `None` when no rate is known, the total
    /// is not known, or the transfer is already complete.
    ///
    /// Never `0` and never an infinity: both are the "not enough history yet" case wearing a
    /// number, and on an hours-long transfer a lying estimate is worse than none.
    pub(in crate::tools::ai_editor::engines) fn remaining(&self, step: u64, total: u64) -> Option<Duration> {
        let rate = self.bytes_per_second()?;
        let left = total.checked_sub(step).filter(|&left| left > 0)?;
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        // Cast justification: as above. `try_from_secs_f64` rejects a non-finite or
        // out-of-range value rather than saturating, so an absurd rate yields no estimate.
        Duration::try_from_secs_f64(left as f64 / rate).ok()
    }
}

/// The SECOND progress level: the single file a model download is transferring (while
/// `step`/`total` stay the overall byte count across the whole plan), or the single file a
/// model LOAD is reading from disk (while `step`/`total` count the load's components).
///
/// Optional on the wire (`dev-docs/flux2_model_download.md` §4) and therefore optional
/// here: a preparation frame carries no file at all, and rendering it as a bar stuck at
/// zero would tell the user a transfer had stalled when nothing had started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct FileProgress {
    /// Bytes of this file already written.
    pub(in crate::tools::ai_editor::engines) step: u64,
    /// Size of this file, `0` when the backend could not state one — which must render
    /// as an indeterminate bar rather than divide.
    pub(in crate::tools::ai_editor::engines) total: u64,
    /// The file's own name, as the backend spelled it.
    pub(in crate::tools::ai_editor::engines) label: String,
    /// The optional `file_component`: which model component a READ is reading this file for
    /// (a wire id: `transformer` / `text_encoder` / `vae`, [`file_component_label`]). `None`
    /// when the frame does not say — a download, or a backend whose load frames describe
    /// the transformer alone.
    pub(in crate::tools::ai_editor::engines) component: Option<String>,
}

/// The localized name of a file read's `file_component` id, `None` for an id this build
/// does not know (the bar then names the file alone). An ABSENT component on a read frame
/// is the transformer: the backends that predate the field sent file frames for it only.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn file_component_label(component: Option<&str>) -> Option<String> {
    match component {
        None | Some("transformer") => Some(t!("cleaning.tools.engine_progress.component_transformer_label").to_string()),
        Some("text_encoder") => Some(t!("cleaning.tools.engine_progress.component_text_encoder_label").to_string()),
        Some("vae") => Some(t!("cleaning.tools.engine_progress.component_vae_label").to_string()),
        Some(_) => None,
    }
}

/// A large-model run the backend finished on a MORE ECONOMICAL step pair than planned,
/// after a memory failure: what it ran as, and why. Steps the build does not know read as
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct RunDegraded {
    pub(in crate::tools::ai_editor::engines) vram: Option<VramStep>,
    pub(in crate::tools::ai_editor::engines) ram: Option<RamStep>,
    pub(in crate::tools::ai_editor::engines) reason: Option<FailureKind>,
}

/// Formats a transfer rate in MiB/s, one decimal.
///
/// MiB and not the GiB the sizes use: a 35 GB transfer runs at tens to hundreds of MiB/s, and
/// in GiB every realistic speed would render as "0.0" or "0.1".
pub(in crate::tools::ai_editor::engines) fn format_mib_per_second(bytes_per_second: f64) -> String {
    // Cast justification: the divisor is an exact power of two and the input is already f64.
    format!("{:.1}", bytes_per_second / (1024.0 * 1024.0))
}

/// Formats a remaining time COARSELY: hours and minutes, minutes, or seconds.
///
/// Deliberately never more precise than that. The number is an extrapolation from a
/// five-second window on a transfer that runs for hours; rendering it to the second would
/// claim an accuracy it does not have and would flicker on every frame.
pub(in crate::tools::ai_editor::engines) fn format_eta(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    if seconds >= 3600 {
        return tf!(
            "cleaning.tools.engine_progress.eta_hours",
            hours = seconds / 3600,
            minutes = (seconds % 3600) / 60
        );
    }
    if seconds >= 60 {
        return tf!("cleaning.tools.engine_progress.eta_minutes", minutes = seconds / 60);
    }
    tf!("cleaning.tools.engine_progress.eta_seconds", seconds = seconds)
}

/// The one line under the bars: the current speed, and the remaining time when one can
/// honestly be given.
///
/// `None` when there is no rate yet — the line is then absent entirely rather than showing a
/// placeholder, because "no estimate" is the honest state at the start of every transfer.
/// Pure, so the "not enough history" rule can be tested without a `Ui`.
///
/// Shown only while the CURRENT frame still feeds the measured counter: a download frame for
/// a download's rate, a frame of any other phase reading the same file for a file read's
/// rate — so a step between two files, or a generate frame without a file level, never shows
/// the last file's speed.
pub(in crate::tools::ai_editor::engines) fn transfer_status(progress: &EngineProgress) -> Option<String> {
    let (step, total) = match progress.rate_source.as_ref()? {
        RateSource::Download if progress.phase == "download" => (progress.step, progress.total),
        RateSource::FileRead { component, label } if progress.phase != "download" => {
            let file = progress.file.as_ref().filter(|file| file.component == *component && file.label == *label)?;
            (file.step, file.total)
        }
        RateSource::Download | RateSource::FileRead { .. } => return None,
    };
    let rate = progress.rate.bytes_per_second()?;
    let speed = tf!("cleaning.tools.engine_progress.speed_status", speed = format_mib_per_second(rate));
    // The estimate is optional even when the speed is known: a plan whose total is not known
    // yet, or one already finished, has no remaining time to report.
    match progress.rate.remaining(step, total) {
        Some(remaining) => Some(tf!(
            "cleaning.tools.engine_progress.speed_and_eta_status",
            speed = speed,
            eta = format_eta(remaining)
        )),
        None => Some(speed),
    }
}

/// Locks the shared progress, recovering it from a poisoned lock: the state is a display
/// copy that every write overwrites whole, so a panicked writer cannot leave it inconsistent
/// in a way the next frame would not repair.
pub(in crate::tools::ai_editor::engines) fn lock_progress(progress: &Mutex<EngineProgress>) -> MutexGuard<'_, EngineProgress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Claims the shared progress for an operation that is about to start and returns ITS claim:
/// the generation the worker carries into every later write, and a FRESH [`CallControl`] (a
/// new operation never inherits the previous one's request id or cancel).
///
/// Called on the GUI thread before the worker is spawned, so two operations started in a
/// row are ordered by construction rather than by whichever thread wins the lock.
pub(in crate::tools::ai_editor::engines) fn begin_progress_generation(progress: &Mutex<EngineProgress>) -> ProgressClaim {
    let mut guard = lock_progress(progress);
    // Wrapping, not saturating: a saturated counter would stop being unique and let a
    // stale worker write into a live run again. 2^64 runs is not a reachable session.
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = true;
    guard.phase = "load".to_string();
    guard.step = 0;
    guard.total = 0;
    guard.label = t!("cleaning.tools.engine_progress.preparing_status").to_string();
    guard.stage = None;
    // The second level belongs to the operation that publishes it: a new claim must not
    // inherit the file the previous download was transferring, nor its measured speed.
    guard.file = None;
    guard.rate = RateEstimator::default();
    guard.rate_source = None;
    let canceller = guard.canceller.clone().unwrap_or_else(|| Arc::new(spawn_backend_cancel) as Canceller);
    let control = Arc::new(CallControl::new(canceller));
    guard.control = Some(Arc::clone(&control));
    ProgressClaim { generation: guard.generation, control }
}

/// Retires the current generation and CANCELS its operation: the bar disappears at once,
/// every later write from the abandoned worker is ignored, and the backend is told to stop —
/// now when the request id is known, else the moment the worker publishes it (sticky,
/// [`CallControl`]). `log_label` names the engine in the cancel's log lines.
pub(in crate::tools::ai_editor::engines) fn retire_progress_generation(progress: &Mutex<EngineProgress>, log_label: &'static str) {
    let control = {
        let mut guard = lock_progress(progress);
        guard.generation = guard.generation.wrapping_add(1);
        guard.active = false;
        guard.control.take()
    };
    // Outside the progress lock: the canceller spawns a thread (or records, in a test).
    if let Some(control) = control {
        control.cancel(log_label);
    }
}

/// Applies `update` to the shared progress only while `generation` still owns it; a
/// write from a retired or superseded operation is dropped.
pub(in crate::tools::ai_editor::engines) fn update_progress(
    progress: &Mutex<EngineProgress>,
    generation: u64,
    update: impl FnOnce(&mut EngineProgress),
) {
    let mut guard = lock_progress(progress);
    if guard.generation != generation {
        return;
    }
    update(&mut guard);
}

/// Writes one decoded progress frame into the shared state under `generation`.
///
/// Every streaming operation of an engine publishes its frames the same way, second
/// level included, so a field added to the wire reaches every bar through one place
/// instead of copies that drift. A frame carrying no `file` CLEARS the second level
/// rather than leaving the previous file on screen. A frame with a byte counter
/// ([`RateSource::of`]) feeds the rate; a counter of ANOTHER source (the next file, or a
/// download after a load) starts a fresh estimator first.
pub(in crate::tools::ai_editor::engines) fn publish_progress_frame(
    progress: &Mutex<EngineProgress>,
    generation: u64,
    frame: ProgressFrame,
) {
    let now = Instant::now();
    update_progress(progress, generation, |state| {
        observe_rate(state, &frame, now);
        state.phase = frame.phase;
        state.step = frame.step;
        state.total = frame.total;
        state.label = frame.label;
        state.stage = frame.stage;
        state.file = frame.file;
    });
}

/// Feeds `frame`'s byte counter, if it has one, into the rate of `state`.
///
/// Only BYTES are fed: a generation's `step` is a step index and a load's `step` a component
/// index, so feeding either would produce a "speed" in bytes per second from step counts.
fn observe_rate(state: &mut EngineProgress, frame: &ProgressFrame, now: Instant) {
    let Some((source, counter)) = RateSource::of(frame) else {
        return;
    };
    if state.rate_source.as_ref() != Some(&source) {
        state.rate = source.estimator();
        state.rate_source = Some(source);
    }
    state.rate.observe(now, counter);
}

/// Asks the backend to stop request `id`, on a worker thread.
///
/// The cancel frame is a socket write behind the client's writer lock, which another
/// thread may be holding for a multi-megabyte request blob — never taken on the GUI
/// thread. A cancel for a finished id is a no-op on the backend. `log_label` names the
/// engine in the log lines (`"FLUX.2 klein"`).
pub(in crate::tools::ai_editor::engines) fn spawn_backend_cancel(id: u64, log_label: &'static str) {
    thread::spawn(move || {
        let client = match backend_ipc::shared_client() {
            Ok(client) => client,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] {log_label} cancel could not reach the backend: {err}"
                ));
                return;
            }
        };
        if let Err(err) = client.cancel(id) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] {log_label} cancel of request {id} failed: {err}"
            ));
        }
    });
}

/// Fraction of a counter for a progress bar: clamped into `0.0..=1.0`, and `0.0` when
/// the total is unknown (`0`) rather than a division by zero.
///
/// Shared by the overall level and the per-file level so the two can never disagree
/// about what an unknown total means.
pub(in crate::tools::ai_editor::engines) fn progress_fraction(step: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    // Cast justification: both counters are either small (steps, module counts) or byte
    // counts of at most tens of gigabytes; f32 loses precision there but the result is a
    // bar fraction, where a byte of drift is invisible.
    (step as f32 / total as f32).clamp(0.0, 1.0)
}

/// The bar fractions the current progress state renders: the OVERALL one, plus the
/// current-file one when the backend reported a second level.
///
/// Pure so the "overall only / both bars" split can be tested without a `Ui`. A frame
/// that carries no file (a preparation phase) yields `None` for the second bar — never a
/// bar pinned at zero, which would read as a stalled transfer.
pub(in crate::tools::ai_editor::engines) fn progress_fractions(progress: &EngineProgress) -> (f32, Option<f32>) {
    let overall = progress_fraction(progress.step, progress.total);
    let file = progress
        .file
        .as_ref()
        .map(|file| progress_fraction(file.step, file.total));
    (overall, file)
}

/// Reads whether a large-model run was degraded and to what (`degraded`, `degrade_reason`,
/// `vram_step_effective`, `ram_step_effective`, top-level — `PROTOCOL.md` §5.10). `None`
/// unless `degraded` is `true`.
pub(in crate::tools::ai_editor::engines) fn parse_degraded(header: &Value) -> Option<RunDegraded> {
    if header.get("degraded").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let text = |name: &str| header.get(name).and_then(Value::as_str);
    Some(RunDegraded {
        vram: text("vram_step_effective").and_then(VramStep::from_wire),
        ram: text("ram_step_effective").and_then(RamStep::from_wire),
        reason: text("degrade_reason").map(FailureKind::from_wire),
    })
}

/// One decoded progress frame of a streaming backend call.
///
/// The four required fields are the shape every streaming method of these engines has
/// always used. `file` is the SECOND level a model download adds
/// (`dev-docs/flux2_model_download.md` §4): optional on the wire, `None` here for every
/// operation that does not publish it, so a consumer that predates it is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct ProgressFrame {
    pub(in crate::tools::ai_editor::engines) phase: String,
    /// The OVERALL counter — steps for a generation, bytes across the whole plan for a
    /// download. Deliberately the old field, so every single-level consumer stays right.
    pub(in crate::tools::ai_editor::engines) step: u64,
    pub(in crate::tools::ai_editor::engines) total: u64,
    pub(in crate::tools::ai_editor::engines) label: String,
    /// The optional stable stage id (`stage`); see [`EngineProgress::stage`].
    pub(in crate::tools::ai_editor::engines) stage: Option<String>,
    pub(in crate::tools::ai_editor::engines) file: Option<FileProgress>,
}

/// Reads one progress frame's header.
///
/// Every field is optional and degrades rather than failing: an absent `phase` reads as
/// `generate` (what a generation sends most of the time), absent counters as `0`, and an
/// absent `file_step`/`file_total` pair as "this frame describes no single file", an absent
/// `file_component` as "no component named" ([`file_component_label`]). A frame
/// that carries EITHER of the two is taken as describing one, because a backend that
/// reports a byte count without a size still has a file in flight to name.
pub(in crate::tools::ai_editor::engines) fn parse_progress_frame(header: &Value) -> ProgressFrame {
    let u64_field = |name: &str| header.get(name).and_then(Value::as_u64);
    let str_field = |name: &str| header.get(name).and_then(Value::as_str);
    let file_step = u64_field("file_step");
    let file_total = u64_field("file_total");
    let file = if file_step.is_none() && file_total.is_none() {
        None
    } else {
        Some(FileProgress {
            step: file_step.unwrap_or(0),
            total: file_total.unwrap_or(0),
            label: str_field("file_label").unwrap_or_default().to_string(),
            // Optional: a blank id is none, exactly like `stage`.
            component: str_field("file_component").filter(|component| !component.trim().is_empty()).map(str::to_string),
        })
    };
    ProgressFrame {
        phase: str_field("phase").unwrap_or("generate").to_string(),
        step: u64_field("step").unwrap_or(0),
        total: u64_field("total").unwrap_or(0),
        label: str_field("label").unwrap_or_default().to_string(),
        // Optional: only a backend that names its stages sends it; an empty one is none.
        stage: str_field("stage").filter(|stage| !stage.trim().is_empty()).map(str::to_string),
        file,
    }
}

/// Streaming call to `method`. Each `progress` frame carries `phase`/`step`/`total`/
/// `label` in the header and no preview blob — the same shape for a generation, a model
/// download and every other long operation, which is why they all go through here. A
/// download additionally fills [`ProgressFrame::file`].
///
/// `on_started` receives the IPC id of the request as soon as it is on the wire; the
/// progress state keeps it so a cancel can stop the work backend-side. This is why the call is
/// built from `begin_call` + `wait_streaming` rather than from the `call_streaming`
/// shorthand, which never exposes the id. `frame_timeout` is per FRAME, not per call:
/// `wait_streaming` restarts the timer on every progress frame, which is what lets a
/// multi-hour model download share one timeout with a generation.
///
/// # Errors
/// The user-facing message of [`map_call_error`]: the backend's refusal, an interrupted
/// call, or an unreachable backend.
pub(in crate::tools::ai_editor::engines) fn stream_call<S, F>(
    method: &'static str,
    header: Value,
    blob: &[u8],
    frame_timeout: Duration,
    on_started: S,
    mut on_progress: F,
) -> Result<(Value, Vec<u8>), String>
where
    S: FnOnce(u64),
    F: FnMut(ProgressFrame),
{
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let handle = client
        .begin_call(method, header, blob)
        .map_err(|err| map_call_error(CallError::Transport(err)))?;
    on_started(handle.id());
    handle
        .wait_streaming(
            |progress_header, _preview_blob| {
                on_progress(parse_progress_frame(progress_header));
            },
            frame_timeout,
        )
        .map_err(map_call_error)
}

/// The user-facing message of a failed backend call: the backend's own refusal verbatim,
/// an interrupted call named as such, and the shared "backend offline" line for a
/// transport failure.
pub(in crate::tools::ai_editor::engines) fn map_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn a_degraded_run_reports_its_effective_steps() {
        let header = json!({
            "degraded": true,
            "degrade_reason": "device_oom",
            "vram_step_effective": "half",
            "ram_step_effective": "keep"
        });
        assert_eq!(
            parse_degraded(&header),
            Some(RunDegraded { vram: Some(VramStep::Half), ram: Some(RamStep::Keep), reason: Some(FailureKind::DeviceOom) })
        );
        assert!(parse_degraded(&json!({ "degraded": false, "vram_step_effective": "min" })).is_none());
        assert!(parse_degraded(&json!({})).is_none());
        let unknown = parse_degraded(&json!({ "degraded": true, "vram_step_effective": "warp" })).expect("degraded");
        assert_eq!(unknown.vram, None, "an unknown step reads as not known");
    }

    #[test]
    fn a_stale_run_cannot_touch_the_progress_of_the_next_one() {
        let progress = Mutex::new(EngineProgress::default());
        let first = begin_progress_generation(&progress).generation;
        update_progress(&progress, first, |state| state.step = 3);
        retire_progress_generation(&progress, "test");
        assert!(!lock_progress(&progress).active);

        let second = begin_progress_generation(&progress).generation;
        assert_ne!(first, second, "each run gets its own generation");
        // The abandoned worker keeps reporting and eventually finishes. None of that
        // may reach the run that replaced it — least of all its `active = false`.
        update_progress(&progress, first, |state| {
            state.step = 99;
            state.active = false;
        });
        let guard = lock_progress(&progress);
        assert!(guard.active, "a stale worker must not erase the live bar");
        assert_eq!(guard.step, 0, "a stale worker must not move the live bar");
    }

    /// The progress split of D6, on the exact frame shapes §4 pins.
    #[test]
    fn a_frame_without_the_file_fields_renders_the_overall_bar_alone() {
        // A preparation frame: the four old fields and nothing else.
        let prepare = parse_progress_frame(&json!({
            "phase": "download", "step": 0, "total": 40_000_000_000u64, "label": "resolving the file list"
        }));
        assert_eq!(prepare.file, None, "no second level was reported");
        let mut progress = EngineProgress::default();
        publish_frame_into(&mut progress, prepare);
        let (overall, file) = progress_fractions(&progress);
        assert!((overall - 0.0).abs() < f32::EPSILON);
        assert_eq!(file, None, "a missing second level draws no second bar");

        // A transfer frame: all six counters.
        let transferring = parse_progress_frame(&json!({
            "phase": "download",
            "step": 12_884_901_888u64, "total": 40_000_000_000u64,
            "label": "transformer/model-00001-of-00002.safetensors",
            "file_step": 3_221_225_472u64, "file_total": 9_800_000_000u64,
            "file_label": "model-00001-of-00002.safetensors"
        }));
        assert_eq!(
            transferring.file,
            Some(FileProgress {
                step: 3_221_225_472,
                total: 9_800_000_000,
                label: "model-00001-of-00002.safetensors".to_string(),
                component: None,
            })
        );
        publish_frame_into(&mut progress, transferring);
        let (overall, file) = progress_fractions(&progress);
        assert!((overall - 0.322_1).abs() < 0.001, "overall was {overall}");
        let file = file.expect("all six counters must draw both bars");
        assert!((file - 0.328_7).abs() < 0.001, "file was {file}");

        // A file whose size the backend could not state: an indeterminate bar, never a
        // division by zero.
        let unsized_file = parse_progress_frame(&json!({
            "phase": "download", "step": 1, "total": 2,
            "file_step": 512u64, "file_label": "config.json"
        }));
        publish_frame_into(&mut progress, unsized_file);
        let (overall, file) = progress_fractions(&progress);
        assert!((overall - 0.5).abs() < f32::EPSILON);
        assert_eq!(file, Some(0.0), "an unknown file size is 0.0, not a panic");

        // And a frame that goes back to having no file clears the second bar rather than
        // leaving the previous file on screen.
        publish_frame_into(
            &mut progress,
            parse_progress_frame(&json!({ "phase": "download", "step": 2, "total": 2 })),
        );
        assert_eq!(progress_fractions(&progress).1, None);
    }

    /// Applies a frame to a progress state the way [`publish_progress_frame`] does, but
    /// without the generation guard — the guard is already pinned by
    /// `a_stale_run_cannot_touch_the_progress_of_the_next_one`.
    fn publish_frame_into(progress: &mut EngineProgress, frame: ProgressFrame) {
        progress.phase = frame.phase;
        progress.step = frame.step;
        progress.total = frame.total;
        progress.label = frame.label;
        progress.file = frame.file;
    }

    // -----------------------------------------------------------------------------------
    // Transfer rate and remaining time
    // -----------------------------------------------------------------------------------

    /// Feeds a scripted DOWNLOAD frame sequence into a progress state exactly as
    /// [`publish_progress_frame`] does, at `origin + offset`.
    ///
    /// `Instant` cannot be constructed at an arbitrary value, but the estimator only ever
    /// looks at DIFFERENCES, so offsets from one origin script it deterministically.
    fn feed_download(progress: &mut EngineProgress, origin: Instant, samples: &[(u64, u64)]) {
        for &(millis, step) in samples {
            let frame = ProgressFrame { phase: "download".to_string(), step, total: progress.total, ..ProgressFrame::default() };
            observe_rate(progress, &frame, origin + Duration::from_millis(millis));
            progress.phase = frame.phase;
            progress.step = step;
        }
    }

    const MIB: u64 = 1024 * 1024;

    /// A steady transfer must report the rate it is actually running at, and an estimate
    /// derived from it.
    #[test]
    fn a_scripted_transfer_reports_the_expected_speed_and_estimate() {
        let origin = Instant::now();
        let mut progress = EngineProgress {
            total: 100 * MIB,
            ..EngineProgress::default()
        };
        // Exactly 20 MiB/s: twenty frames, 100 ms apart, 2 MiB each. The sequence spans
        // 2 s deliberately — one second of history is BELOW `RATE_MIN_SPAN` and
        // correctly yields no rate at all, which the "too little history" test pins.
        let samples: Vec<(u64, u64)> = (0..=20).map(|i| (i * 100, i * 2 * MIB)).collect();
        feed_download(&mut progress, origin, &samples);
        assert!(Duration::from_millis(2000) >= RATE_MIN_SPAN);

        let rate = progress
            .rate
            .bytes_per_second()
            .expect("two seconds of history must yield a rate");
        // Tolerance 1%: the window arithmetic is exact here, and the allowance exists only
        // so the assertion does not depend on f64's last bits.
        let expected = 20.0 * MIB as f64;
        assert!(
            (rate - expected).abs() < expected * 0.01,
            "expected ~{expected} B/s, got {rate}"
        );
        assert_eq!(format_mib_per_second(rate), "20.0");

        // 40 MiB transferred of 100 MiB, so 60 MiB left at 20 MiB/s is 3 s.
        assert_eq!(progress.step, 40 * MIB);
        let remaining = progress
            .rate
            .remaining(progress.step, progress.total)
            .expect("a known rate and a known total give an estimate");
        assert_eq!(remaining.as_secs(), 3);
        assert_eq!(
            format_eta(remaining),
            tf!("cleaning.tools.engine_progress.eta_seconds", seconds = 3)
        );

        // The line exists and names both halves.
        assert!(transfer_status(&progress).is_some());

        // The coarse formatter's three bands.
        assert_eq!(
            format_eta(Duration::from_secs(45)),
            tf!("cleaning.tools.engine_progress.eta_seconds", seconds = 45)
        );
        assert_eq!(
            format_eta(Duration::from_secs(15 * 60)),
            tf!("cleaning.tools.engine_progress.eta_minutes", minutes = 15)
        );
        assert_eq!(
            format_eta(Duration::from_secs(2 * 3600 + 15 * 60)),
            tf!("cleaning.tools.engine_progress.eta_hours", hours = 2, minutes = 15)
        );
    }

    /// At the start of a transfer there is not enough history for any number to mean
    /// something, and the honest answer is silence — not "∞", not "0 с".
    #[test]
    fn too_little_history_yields_no_speed_and_no_estimate() {
        let origin = Instant::now();
        let mut progress = EngineProgress {
            total: 100 * MIB,
            ..EngineProgress::default()
        };

        // Nothing at all.
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(progress.rate.remaining(0, progress.total), None);
        assert_eq!(transfer_status(&progress), None);

        // A single frame is not a rate.
        feed_download(&mut progress, origin, &[(0, 4 * MIB)]);
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(transfer_status(&progress), None);

        // Two frames 100 ms apart are a burst, not a measurement: the window is shorter
        // than the minimum span, so still nothing.
        feed_download(&mut progress, origin, &[(100, 8 * MIB)]);
        assert!(Duration::from_millis(100) < RATE_MIN_SPAN);
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(transfer_status(&progress), None);

        // Once the window covers the minimum span, a rate appears.
        feed_download(&mut progress, origin, &[(2000, 40 * MIB)]);
        assert!(progress.rate.bytes_per_second().is_some());

        // A counter that does not move is not a rate of zero: it reports nothing, so no
        // estimate can be computed from it and none is shown.
        let mut stalled = EngineProgress {
            total: 100 * MIB,
            ..EngineProgress::default()
        };
        feed_download(&mut stalled, origin, &[(0, 8 * MIB), (2000, 8 * MIB), (4000, 8 * MIB)]);
        assert_eq!(stalled.rate.bytes_per_second(), None);
        assert_eq!(stalled.rate.remaining(8 * MIB, 100 * MIB), None);

        // A finished transfer has no remaining time either — never "0 с" beside a full bar.
        let mut done = EngineProgress {
            total: 40 * MIB,
            ..EngineProgress::default()
        };
        feed_download(&mut done, origin, &[(0, 0), (2000, 40 * MIB)]);
        assert!(done.rate.bytes_per_second().is_some());
        assert_eq!(done.rate.remaining(40 * MIB, 40 * MIB), None);
        // An unknown total likewise yields a speed but no estimate.
        assert_eq!(done.rate.remaining(40 * MIB, 0), None);
    }

    /// The documented wart: a resumed file whose length check fails is refetched from zero,
    /// so the OVERALL counter moves backwards exactly once for that file
    /// (`ipc/PROTOCOL.md`, `download.start`). It must never render as a negative or absurd
    /// rate, and must never panic.
    #[test]
    fn a_backwards_step_discards_the_window_and_never_renders_a_wild_rate() {
        let origin = Instant::now();
        let mut progress = EngineProgress {
            total: 100 * MIB,
            ..EngineProgress::default()
        };
        // A steady climb to 60 MiB...
        feed_download(
            &mut progress,
            origin,
            &[(0, 0), (1000, 20 * MIB), (2000, 40 * MIB), (3000, 60 * MIB)],
        );
        let before = progress
            .rate
            .bytes_per_second()
            .expect("the climb established a rate");
        assert!(before > 0.0);

        // ...then the refetch: the file restarts, so the overall counter drops.
        feed_download(&mut progress, origin, &[(3100, 45 * MIB)]);
        // The window was discarded, so nothing is claimed until it refills. Critically, the
        // answer is None rather than a negative number.
        assert_eq!(
            progress.rate.bytes_per_second(),
            None,
            "the samples before a restart describe bytes being sent again"
        );
        assert_eq!(progress.rate.remaining(45 * MIB, 100 * MIB), None);
        assert_eq!(transfer_status(&progress), None);

        // The window refills from the new baseline and reports a SANE rate again — not one
        // inflated or deflated by the seam.
        feed_download(
            &mut progress,
            origin,
            &[(4100, 65 * MIB), (5100, 85 * MIB)],
        );
        let after = progress
            .rate
            .bytes_per_second()
            .expect("the window refilled after the restart");
        let expected = 20.0 * MIB as f64;
        assert!(
            (after - expected).abs() < expected * 0.01,
            "expected ~{expected} B/s after the restart, got {after}"
        );
        assert!(after.is_finite() && after > 0.0);
        // And the estimate that follows is a real duration, not a wild number.
        let remaining = progress
            .rate
            .remaining(85 * MIB, 100 * MIB)
            .expect("a sane rate gives a sane estimate");
        assert!(remaining.as_secs() <= 2, "got {remaining:?}");

        // A drop to zero is the same case and must be equally survivable.
        let mut restarted = EngineProgress::default();
        feed_download(&mut restarted, origin, &[(0, 60 * MIB), (2000, 80 * MIB), (2100, 0)]);
        assert_eq!(restarted.rate.bytes_per_second(), None);
        assert_eq!(restarted.rate.remaining(0, 100 * MIB), None);
    }

    /// The optional per-file fields are exactly that: a preparation frame carrying only the
    /// overall level must still drive the speed.
    #[test]
    fn a_frame_without_the_file_fields_still_advances_the_speed() {
        let origin = Instant::now();
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let generation = begin_progress_generation(&progress).generation;

        // Frames as the wire sends them, half of them without the per-file trio.
        for (index, millis) in [(0u64, 0u64), (1, 2000), (2, 4000)] {
            let mut header = json!({
                "phase": "download",
                "step": index * 20 * MIB,
                "total": 100 * MIB,
                "label": "transformer/shard.safetensors"
            });
            // Only the middle frame describes a file; the others are preparation phases.
            if index == 1 {
                let object = header.as_object_mut().expect("object");
                object.insert("file_step".to_string(), json!(4 * MIB));
                object.insert("file_total".to_string(), json!(9 * MIB));
                object.insert("file_label".to_string(), json!("shard.safetensors"));
            }
            let frame = parse_progress_frame(&header);
            // Fed through the estimator the same way `publish_progress_frame` does, with a
            // scripted clock instead of the wall clock.
            let mut guard = lock_progress(&progress);
            observe_rate(&mut guard, &frame, origin + Duration::from_millis(millis));
            guard.phase = frame.phase;
            guard.step = frame.step;
            guard.total = frame.total;
            guard.file = frame.file;
            drop(guard);
        }
        assert_eq!(generation, lock_progress(&progress).generation);

        let guard = lock_progress(&progress);
        // The last frame carried no file fields, so there is no second bar...
        assert_eq!(guard.file, None);
        assert_eq!(progress_fractions(&guard).1, None);
        // ...and the speed is nevertheless known, from the overall level alone.
        let rate = guard
            .rate
            .bytes_per_second()
            .expect("the overall level alone must drive the rate");
        let expected = 10.0 * MIB as f64;
        assert!(
            (rate - expected).abs() < expected * 0.01,
            "expected ~{expected} B/s, got {rate}"
        );
        assert!(transfer_status(&guard).is_some());
    }

    /// The numbers belong to ONE transfer and must not outlive it.
    #[test]
    fn a_new_operation_does_not_inherit_the_previous_transfer_s_speed() {
        let origin = Instant::now();
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        begin_progress_generation(&progress);
        {
            let mut guard = lock_progress(&progress);
            feed_download(&mut guard, origin, &[(0, 0), (2000, 40 * MIB)]);
            guard.total = 100 * MIB;
            assert!(guard.rate.bytes_per_second().is_some());
        }

        // Claiming the bar for the next operation clears the history, so a generation
        // started after a download cannot show the download's speed.
        begin_progress_generation(&progress);
        let guard = lock_progress(&progress);
        assert_eq!(guard.rate.bytes_per_second(), None);
        assert_eq!(transfer_status(&guard), None);
        drop(guard);

        // A retired bar is inactive, which is what stops the line being drawn at all; the
        // renderer returns before reaching it.
        retire_progress_generation(&progress, "test");
        assert!(!lock_progress(&progress).active);
    }

    /// Feeds one scripted LOAD frame reading `label` (for `component`) at `origin + millis`,
    /// exactly as [`publish_progress_frame`] applies it.
    fn feed_load(progress: &mut EngineProgress, origin: Instant, millis: u64, component: Option<&str>, label: &str, file_step: u64, file_total: u64) {
        let mut header = json!({ "phase": "load", "step": 2, "total": 5, "label": "loading", "file_step": file_step, "file_total": file_total, "file_label": label });
        if let Some(component) = component {
            header.as_object_mut().expect("object").insert("file_component".to_string(), json!(component));
        }
        let frame = parse_progress_frame(&header);
        observe_rate(progress, &frame, origin + Duration::from_millis(millis));
        progress.phase = frame.phase;
        progress.step = frame.step;
        progress.total = frame.total;
        progress.file = frame.file;
    }

    /// A load's per-file byte counter drives a ONE-second disk-read rate (a load's `step` is a
    /// component index and never feeds it); jittery chunk frames average out instead of
    /// jumping; the next file or another component starts a fresh window; a load frame
    /// between files shows no speed.
    #[test]
    fn a_load_reads_its_speed_from_the_file_counter_and_restarts_per_file() {
        let origin = Instant::now();
        let mut progress = EngineProgress::default();
        // Five frames a second at an AVERAGE of 100 MiB/s, alternating 60 / 140 MiB chunks.
        let mut read = 0;
        for index in 0..=10u64 {
            feed_load(&mut progress, origin, index * 200, None, "flux-2-klein-9b.safetensors", read, 2048 * MIB);
            read += if index % 2 == 0 { 12 * MIB } else { 28 * MIB };
        }
        assert_eq!(progress.rate_source, Some(RateSource::FileRead { component: None, label: "flux-2-klein-9b.safetensors".to_string() }));
        let rate = progress.rate.bytes_per_second().expect("two seconds of reads give a rate");
        let expected = 100.0 * MIB as f64;
        // The one-second window spans five or six alternating chunks, so the reading stays
        // within a fifth of the average rather than swinging between 60 and 140.
        assert!((rate - expected).abs() < expected * 0.2, "expected ~{expected} B/s, got {rate}");
        assert!(transfer_status(&progress).is_some(), "a load reading a file shows its speed");

        // The text encoder's file: a new source, so a new (empty) window, then its own rate.
        feed_load(&mut progress, origin, 2400, Some("text_encoder"), "qwen3-8b.gguf", 0, 1024 * MIB);
        assert_eq!(progress.rate.bytes_per_second(), None, "the previous file's samples are gone");
        assert_eq!(transfer_status(&progress), None);
        feed_load(&mut progress, origin, 3400, Some("text_encoder"), "qwen3-8b.gguf", 50 * MIB, 1024 * MIB);
        let rate = progress.rate.bytes_per_second().expect("one second of the encoder file");
        assert!((rate - 50.0 * MIB as f64).abs() < 1.0, "got {rate}");

        // A load step without a file level shows no speed (the rate is kept for the file).
        let between = parse_progress_frame(&json!({ "phase": "load", "step": 3, "total": 5, "label": "vae" }));
        observe_rate(&mut progress, &between, origin + Duration::from_millis(3600));
        progress.phase = between.phase;
        progress.file = between.file;
        assert_eq!(transfer_status(&progress), None);

        // A generation's step counts never feed it either.
        let generate = parse_progress_frame(&json!({ "phase": "generate", "step": 4, "total": 4 }));
        assert_eq!(RateSource::of(&generate), None);
    }

    /// A frame of ANY phase that carries the byte fields reads a file (the encode phase, a
    /// denoise re-reading disk-tier blocks): the second bar and the speed follow it, the
    /// overall generate counter stays the step counter, and the next frame without the
    /// byte fields hides both.
    #[test]
    fn a_generate_frame_with_the_byte_fields_shows_the_file_read() {
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let generation = begin_progress_generation(&progress).generation;
        let origin = Instant::now();
        for (millis, read) in [(0u64, 0u64), (500, 40 * MIB), (1000, 80 * MIB)] {
            let frame = parse_progress_frame(&json!({
                "phase": "generate", "step": 3, "total": 8, "label": "denoise",
                "file_step": read, "file_total": 400 * MIB, "file_label": "flux-2-klein-9b.safetensors", "file_component": "transformer"
            }));
            let mut guard = lock_progress(&progress);
            assert_eq!(guard.generation, generation);
            observe_rate(&mut guard, &frame, origin + Duration::from_millis(millis));
            publish_frame_into(&mut guard, frame);
        }
        {
            let guard = lock_progress(&progress);
            let (overall, file) = progress_fractions(&guard);
            assert!((overall - 3.0 / 8.0).abs() < 1e-6, "the generate bar keeps its step counter");
            assert!((file.expect("a file bar") - 0.2).abs() < 1e-6);
            let rate = guard.rate.bytes_per_second().expect("one second of re-reads");
            assert!((rate - 80.0 * MIB as f64).abs() < 1.0, "got {rate}");
            assert!(transfer_status(&guard).is_some(), "a generate frame reading a file shows its speed");
        }
        // The next denoise step without the byte fields: no file bar, no speed line.
        publish_progress_frame(&progress, generation, parse_progress_frame(&json!({ "phase": "generate", "step": 4, "total": 8 })));
        let guard = lock_progress(&progress);
        assert_eq!(progress_fractions(&guard).1, None);
        assert_eq!(transfer_status(&guard), None);
    }

    /// `file_component` is optional: absent or blank is none, which a load names as the
    /// transformer; a known id is localized and an unknown one names nothing.
    #[test]
    fn the_file_component_is_optional_and_localized() {
        let _locale = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let file = |header: Value| parse_progress_frame(&header).file.expect("a file level");
        assert_eq!(file(json!({ "phase": "load", "file_step": 1, "file_label": "a" })).component, None);
        assert_eq!(file(json!({ "phase": "load", "file_step": 1, "file_component": " " })).component, None);
        assert_eq!(file(json!({ "phase": "load", "file_step": 1, "file_component": "text_encoder" })).component.as_deref(), Some("text_encoder"));
        assert_eq!(file_component_label(None).as_deref(), Some("Transformer"));
        assert_eq!(file_component_label(Some("transformer")).as_deref(), Some("Transformer"));
        assert_eq!(file_component_label(Some("text_encoder")).as_deref(), Some("Text encoder"));
        assert_eq!(file_component_label(Some("vae")).as_deref(), Some("VAE"));
        assert_eq!(file_component_label(Some("warp_core")), None);
    }

    /// The optional `stage` id travels from the frame into the shared state and is cleared by
    /// the next claim and by a frame without one, so a localized stage name never outlives the
    /// frame that carried it. A frame without it (FLUX.2) parses exactly as before.
    #[test]
    fn the_stage_id_is_optional_and_never_outlives_its_frame() {
        let staged = parse_progress_frame(&json!({ "phase": "load", "step": 1, "total": 11, "label": "загрузка", "stage": "dit_load" }));
        assert_eq!(staged.stage.as_deref(), Some("dit_load"));
        assert_eq!(parse_progress_frame(&json!({ "phase": "load", "stage": "  " })).stage, None, "a blank stage is none");
        assert_eq!(parse_progress_frame(&json!({ "phase": "generate", "step": 1, "total": 4 })).stage, None);

        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let generation = begin_progress_generation(&progress).generation;
        publish_progress_frame(&progress, generation, staged);
        assert_eq!(lock_progress(&progress).stage.as_deref(), Some("dit_load"));
        publish_progress_frame(&progress, generation, parse_progress_frame(&json!({ "phase": "generate", "step": 2, "total": 4 })));
        assert_eq!(lock_progress(&progress).stage, None, "a frame without a stage clears it");
        publish_progress_frame(&progress, generation, parse_progress_frame(&json!({ "phase": "generate", "stage": "denoise" })));
        begin_progress_generation(&progress);
        assert_eq!(lock_progress(&progress).stage, None, "a new claim starts without the previous stage");
    }

    /// A progress whose controls record every backend cancel instead of sending it.
    fn recording_progress() -> (Arc<Mutex<EngineProgress>>, Arc<Mutex<Vec<u64>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&sent);
        let progress = EngineProgress {
            canceller: Some(Arc::new(move |id, _label| record.lock().expect("record").push(id))),
            ..EngineProgress::default()
        };
        (Arc::new(Mutex::new(progress)), sent)
    }

    /// The race the handshake exists for, made deterministic: the worker is held BEFORE its
    /// request id exists (where `begin_call` would block), the user cancels, then the worker
    /// is released and publishes the id — exactly one backend cancel goes out, for that id.
    /// The next claim gets a fresh control: it inherits neither the id nor the cancel.
    #[test]
    fn a_cancel_before_the_request_id_still_cancels_the_backend_exactly_once() {
        let (progress, sent) = recording_progress();
        let claim = begin_progress_generation(&progress);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let worker_claim = claim.clone();
        let worker = std::thread::spawn(move || {
            release_rx.recv().expect("released");
            // `begin_call` returned: the id is published through `on_started`.
            worker_claim.publish_request_id(41);
        });
        retire_progress_generation(&progress, "test");
        assert!(sent.lock().expect("record").is_empty(), "no id yet: nothing can be sent");
        assert!(claim.control.cancel_requested());
        release_tx.send(()).expect("release");
        worker.join().expect("worker");
        assert_eq!(*sent.lock().expect("record"), vec![41], "the late id is cancelled at once");
        // Repeats change nothing: one cancel per operation.
        claim.control.cancel("test");
        claim.publish_request_id(41);
        assert_eq!(sent.lock().expect("record").len(), 1);

        let next = begin_progress_generation(&progress);
        assert!(!next.control.cancel_requested(), "a new operation starts uncancelled");
        next.publish_request_id(42);
        assert_eq!(sent.lock().expect("record").len(), 1, "and its id is not cancelled");
        retire_progress_generation(&progress, "test");
        assert_eq!(*sent.lock().expect("record"), vec![41, 42], "a cancel after the id is sent at once");
    }

    /// An abandoned job keeps the engine busy until its worker answers or ends.
    #[test]
    fn an_abandoned_job_is_busy_until_its_worker_ends() {
        let mut jobs = AbandonedJobs::default();
        assert!(!jobs.poll());
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        jobs.push(rx);
        assert!(jobs.poll() && jobs.busy(), "still running");
        tx.send(1).expect("send");
        assert!(!jobs.poll(), "answered");
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        jobs.push(rx);
        drop(tx);
        assert!(!jobs.poll(), "a crashed worker settles too");
    }
}
