/*
File: cleaning/tools/ai_editor/engines/flux2_klein/progress.rs

Purpose:
The shared progress state of every long FLUX.2 klein operation — a run, a download, a
prompt-cache build — together with the generation counter that keeps a retired job from
writing into the next one's bar, the transfer-rate estimator, and the frames the backend
streams into them.

Main responsibilities:
- own `Flux2Progress` and the `Mutex` discipline around it (`lock_progress`,
  `begin_progress_generation`, `retire_progress_generation`, `update_progress`,
  `publish_progress_frame`);
- keep a bounded sliding window of transfer samples and derive speed and ETA from it
  (`Flux2RateEstimator`, `flux2_transfer_status`);
- parse a streamed progress frame and the applied-flags header the backend answers with;
- format the byte, rate and duration strings the bar shows.

Key structures:
- `Flux2Progress`, `Flux2RateEstimator`, `Flux2FileProgress`, `Flux2ProgressFrame`
- `Flux2AppliedFlags`, `Flux2RunOutcome`, `Flux2JobResult`

Key functions:
- `begin_progress_generation()`, `retire_progress_generation()`, `publish_progress_frame()`
- `flux2_progress_fraction()`, `flux2_progress_fractions()`, `flux2_transfer_status()`
- `parse_flux2_progress_frame()`, `parse_applied_flags()`, `spawn_flux2_cancel()`
- `format_gib()`, `format_mib_per_second()`, `format_eta()`

Notes:
The GENERATION is the invariant: a frame whose generation no longer matches the live one
is dropped, so a cancelled job can never move the bar of the job that replaced it. The
DRAWING of the bar (`draw_flux2_progress_ui`) lives in `ui/progress.rs`.
*/

use super::*;

/// Live progress shared between the run worker and the engine's own panel.
///
/// ONE instance is owned by the engine and reused by every run, so it is claimed by
/// GENERATION: [`begin_progress_generation`] hands the next number to a starting run
/// and every later write from an older one — including its terminal `active = false` —
/// is dropped by [`update_progress`]. Without that, a cancelled worker finishing a
/// minute later would erase the bar of the run that replaced it and leave a bare
/// spinner until that run ended.
#[derive(Default)]
pub(super) struct Flux2Progress {
    /// The run that currently owns every other field. `0` = no run has started yet,
    /// which no worker can ever carry.
    pub(super) generation: u64,
    pub(super) active: bool,
    pub(super) phase: String,
    pub(super) step: u64,
    pub(super) total: u64,
    pub(super) label: String,
    /// The file the model download is transferring right now, `None` for every
    /// operation and every phase that has no second level. See [`Flux2FileProgress`].
    pub(super) file: Option<Flux2FileProgress>,
    /// Transfer rate over the OVERALL counter, fed only by `download` frames. Reset by
    /// [`begin_progress_generation`], so a new operation never inherits the previous
    /// transfer's speed.
    pub(super) rate: Flux2RateEstimator,
    /// IPC id of the in-flight request of `generation`, published by the worker as
    /// soon as the request is on the wire and taken by a cancel, which needs it to
    /// stop the backend instead of merely dropping the answer.
    pub(super) cancel_id: Option<u64>,
}

/// Wall-clock span the transfer-rate estimator averages over.
///
/// A TIME window rather than a fixed number of frames: the backend throttles progress to
/// roughly ten frames per second AND at least 4 MB apart, so the frame CADENCE varies with
/// the line speed — a fast link reports every 4 MB, a slow one ten times a second. Averaging
/// over N frames would therefore average over a different amount of wall time at every
/// speed, while this averages over a known five seconds whatever the cadence.
pub(super) const FLUX2_RATE_WINDOW: Duration = Duration::from_secs(5);

/// Shortest span the window must cover before a rate is reported at all.
///
/// Two consecutive frames can be 100 ms apart and 4 MB wide, which is a 40 MB/s reading
/// taken from a single burst; on a transfer measured in hours an estimate built from that
/// is worse than no estimate. Nothing is shown until the window spans this much real time.
pub(super) const FLUX2_RATE_MIN_SPAN: Duration = Duration::from_millis(1500);

/// Rolling transfer-rate estimator over the OVERALL byte counter.
///
/// Fed from `step`/`total` and never from the per-file counters: the per-file counter resets
/// at every file boundary, which on a 35 GB plan would make the rate jump to nonsense
/// several times per gigabyte.
///
/// The rate is the total byte delta across the retained window divided by the wall time it
/// spans — a moving average over [`FLUX2_RATE_WINDOW`]. An exponential average was the
/// alternative and was rejected: its smoothing factor only means something relative to a
/// fixed sample interval, and this stream's interval is set by whichever of the backend's
/// two throttles binds, which changes with the line speed.
#[derive(Debug, Clone, Default)]
pub(super) struct Flux2RateEstimator {
    /// `(observed_at, overall step)`, oldest first. Bounded by the window, so at ten frames
    /// per second it holds about fifty entries.
    pub(super) samples: VecDeque<(Instant, u64)>,
}

impl Flux2RateEstimator {
    /// Records one overall-byte observation.
    ///
    /// A BACKWARDS step discards the whole window and starts a new one from this sample.
    /// That is not defensive coding for an impossible case: a resumed file whose final
    /// length is wrong is refetched from zero exactly once, so the overall counter really
    /// does move backwards once per such file (`ipc/PROTOCOL.md`, `download.start`). The
    /// samples before the restart describe bytes that are being sent again, so averaging
    /// across the seam would report a rate that never happened; dropping them costs a few
    /// seconds of "no estimate" and is the only honest answer.
    pub(super) fn observe(&mut self, now: Instant, step: u64) {
        if self.samples.back().is_some_and(|&(_, last)| step < last) {
            self.samples.clear();
        }
        self.samples.push_back((now, step));
        // Keep one sample older than the window so the span is never shorter than it.
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|&(t, _)| now.duration_since(t) > FLUX2_RATE_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// Bytes per second averaged over the retained window, or `None` while there is not
    /// enough history for the number to mean anything.
    ///
    /// `None` covers every degenerate case rather than reporting a number: fewer than two
    /// samples, a window shorter than [`FLUX2_RATE_MIN_SPAN`], and a window in which the
    /// byte counter did not move.
    pub(super) fn bytes_per_second(&self) -> Option<f64> {
        let (first_at, first_step) = *self.samples.front()?;
        let (last_at, last_step) = *self.samples.back()?;
        let span = last_at.duration_since(first_at);
        if self.samples.len() < 2 || span < FLUX2_RATE_MIN_SPAN {
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
    pub(super) fn remaining(&self, step: u64, total: u64) -> Option<Duration> {
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

/// The SECOND progress level: the single file a model download is transferring, while
/// `step`/`total` above stay the overall byte count across the whole plan.
///
/// Optional on the wire (`dev-docs/flux2_model_download.md` §4) and therefore optional
/// here: a preparation frame carries no file at all, and rendering it as a bar stuck at
/// zero would tell the user a transfer had stalled when nothing had started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2FileProgress {
    /// Bytes of this file already written.
    pub(super) step: u64,
    /// Size of this file, `0` when the backend could not state one — which must render
    /// as an indeterminate bar rather than divide.
    pub(super) total: u64,
    /// The file's own name, as the backend spelled it.
    pub(super) label: String,
}

/// Memory flags the backend ACTUALLY used for a finished run.
///
/// They may differ from what was requested: an out-of-memory failure during the VAE
/// decode is recovered by retrying it with cheaper settings, and the recovered values
/// come back here so the next run starts from them instead of hitting the same wall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Flux2AppliedFlags {
    pub(super) unload_transformer_before_vae: bool,
    pub(super) vae_tiling: bool,
    pub(super) vae_slicing: bool,
    pub(super) unload_text_encoder_after_encode: bool,
    pub(super) text_encoder_fp8: bool,
}

/// One finished generation: the regenerated region plus what the backend reports about
/// how it got there.
pub(super) struct Flux2RunOutcome {
    pub(super) image: egui::ColorImage,
    /// The backend hit an out-of-memory failure during the VAE decode and recovered
    /// from it without re-running the denoising.
    pub(super) oom_recovered: bool,
    /// Present when the backend reported the flags it ended up using.
    pub(super) applied: Option<Flux2AppliedFlags>,
}

/// Message the run worker sends back. `source` is the region the run started from and
/// becomes the undo entry.
pub(super) struct Flux2JobResult {
    pub(super) source: egui::ColorImage,
    pub(super) result: Result<Flux2RunOutcome, String>,
}

/// Formats a byte count as GIBIBYTES (2^30 bytes) with one decimal.
///
/// The unit itself lives in the locale template — «ГиБ» / `GiB` / `Gio` — so it can be
/// translated, and every template that consumes this function must name that unit and
/// not a decimal gigabyte.
pub(super) fn format_gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Formats a transfer rate in MiB/s, one decimal.
///
/// MiB and not the GiB the sizes use: a 35 GB transfer runs at tens to hundreds of MiB/s, and
/// in GiB every realistic speed would render as "0.0" or "0.1".
pub(super) fn format_mib_per_second(bytes_per_second: f64) -> String {
    // Cast justification: the divisor is an exact power of two and the input is already f64.
    format!("{:.1}", bytes_per_second / (1024.0 * 1024.0))
}

/// Formats a remaining time COARSELY: hours and minutes, minutes, or seconds.
///
/// Deliberately never more precise than that. The number is an extrapolation from a
/// five-second window on a transfer that runs for hours; rendering it to the second would
/// claim an accuracy it does not have and would flicker on every frame.
pub(super) fn format_eta(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    if seconds >= 3600 {
        return tf!(
            "cleaning.tools.flux2_klein.download.eta_hours",
            hours = seconds / 3600,
            minutes = (seconds % 3600) / 60
        );
    }
    if seconds >= 60 {
        return tf!(
            "cleaning.tools.flux2_klein.download.eta_minutes",
            minutes = seconds / 60
        );
    }
    tf!(
        "cleaning.tools.flux2_klein.download.eta_seconds",
        seconds = seconds
    )
}

/// The one line under the bars: the current speed, and the remaining time when one can
/// honestly be given.
///
/// `None` when there is no rate yet — the line is then absent entirely rather than showing a
/// placeholder, because "no estimate" is the honest state at the start of every transfer.
/// Pure, so the "not enough history" rule can be tested without a `Ui`.
pub(super) fn flux2_transfer_status(progress: &Flux2Progress) -> Option<String> {
    let rate = progress.rate.bytes_per_second()?;
    let speed = tf!(
        "cleaning.tools.flux2_klein.download.speed_status",
        speed = format_mib_per_second(rate)
    );
    // The estimate is optional even when the speed is known: a plan whose total is not known
    // yet, or one already finished, has no remaining time to report.
    match progress.rate.remaining(progress.step, progress.total) {
        Some(remaining) => Some(tf!(
            "cleaning.tools.flux2_klein.download.speed_and_eta_status",
            speed = speed,
            eta = format_eta(remaining)
        )),
        None => Some(speed),
    }
}

pub(super) fn lock_progress(progress: &Mutex<Flux2Progress>) -> MutexGuard<'_, Flux2Progress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Claims the shared progress for a run that is about to start and returns ITS
/// generation, which the worker must carry into every later write.
///
/// Called on the GUI thread before the worker is spawned, so two runs started in a row
/// are ordered by construction rather than by whichever thread wins the lock.
pub(super) fn begin_progress_generation(progress: &Mutex<Flux2Progress>) -> u64 {
    let mut guard = lock_progress(progress);
    // Wrapping, not saturating: a saturated counter would stop being unique and let a
    // stale worker write into a live run again. 2^64 runs is not a reachable session.
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = true;
    guard.phase = "load".to_string();
    guard.step = 0;
    guard.total = 0;
    guard.label = t!("cleaning.tools.flux2_klein.preparing_status").to_string();
    // The second level belongs to the operation that publishes it: a new claim must not
    // inherit the file the previous download was transferring, nor its measured speed.
    guard.file = None;
    guard.rate = Flux2RateEstimator::default();
    guard.cancel_id = None;
    guard.generation
}

/// Retires the current generation: the bar disappears at once and every later write
/// from the abandoned worker is ignored.
///
/// Returns the IPC id of the abandoned request when it had already reached the wire,
/// so the caller can cancel it backend-side instead of leaving it computing.
pub(super) fn retire_progress_generation(progress: &Mutex<Flux2Progress>) -> Option<u64> {
    let mut guard = lock_progress(progress);
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = false;
    guard.cancel_id.take()
}

/// Applies `update` to the shared progress only while `generation` still owns it; a
/// write from a retired or superseded run is dropped.
pub(super) fn update_progress(
    progress: &Mutex<Flux2Progress>,
    generation: u64,
    update: impl FnOnce(&mut Flux2Progress),
) {
    let mut guard = lock_progress(progress);
    if guard.generation != generation {
        return;
    }
    update(&mut guard);
}

/// Writes one decoded progress frame into the shared state under `generation`.
///
/// Every streaming operation of this engine publishes its frames the same way, second
/// level included, so a field added to the wire reaches all four bars through one place
/// instead of four copies that drift. A frame carrying no `file` CLEARS the second level
/// rather than leaving the previous file on screen.
pub(super) fn publish_progress_frame(
    progress: &Mutex<Flux2Progress>,
    generation: u64,
    frame: Flux2ProgressFrame,
) {
    let now = Instant::now();
    update_progress(progress, generation, |state| {
        // Only a download's counters are BYTES. A generation's `step` is a step index, so
        // feeding it here would produce a "speed" in bytes per second from step counts.
        if frame.phase == "download" {
            state.rate.observe(now, frame.step);
        }
        state.phase = frame.phase;
        state.step = frame.step;
        state.total = frame.total;
        state.label = frame.label;
        state.file = frame.file;
    });
}

/// Asks the backend to stop request `id`, on a worker thread.
///
/// The cancel frame is a socket write behind the client's writer lock, which another
/// thread may be holding for a multi-megabyte request blob — never taken on the GUI
/// thread. A cancel for a finished id is a no-op on the backend.
pub(super) fn spawn_flux2_cancel(id: u64) {
    thread::spawn(move || {
        let client = match backend_ipc::shared_client() {
            Ok(client) => client,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein cancel could not reach the backend: {err}"
                ));
                return;
            }
        };
        if let Err(err) = client.cancel(id) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] FLUX.2 klein cancel of request {id} failed: {err}"
            ));
        }
    });
}

/// Fraction of a counter for a progress bar: clamped into `0.0..=1.0`, and `0.0` when
/// the total is unknown (`0`) rather than a division by zero.
///
/// Shared by the overall level and the per-file level so the two can never disagree
/// about what an unknown total means.
pub(super) fn flux2_progress_fraction(step: u64, total: u64) -> f32 {
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
pub(super) fn flux2_progress_fractions(progress: &Flux2Progress) -> (f32, Option<f32>) {
    let overall = flux2_progress_fraction(progress.step, progress.total);
    let file = progress
        .file
        .as_ref()
        .map(|file| flux2_progress_fraction(file.step, file.total));
    (overall, file)
}

/// Reads the `applied` object of a generation response.
///
/// All FIVE memory flags must be present: `None` when the backend reported no `applied`
/// at all (an older build) and also when it reported only some of them, so the tool's
/// own settings are then left exactly as the user set them rather than half-overwritten.
pub(super) fn parse_applied_flags(header: &Value) -> Option<Flux2AppliedFlags> {
    let applied = header.get("applied")?;
    let flag = |name: &str| applied.get(name).and_then(Value::as_bool);
    Some(Flux2AppliedFlags {
        unload_transformer_before_vae: flag("unload_transformer_before_vae")?,
        vae_tiling: flag("vae_tiling")?,
        vae_slicing: flag("vae_slicing")?,
        unload_text_encoder_after_encode: flag("unload_text_encoder_after_encode")?,
        text_encoder_fp8: flag("text_encoder_fp8")?,
    })
}

/// One decoded progress frame of a streaming FLUX.2 call.
///
/// The four required fields are the shape every streaming method of this engine has
/// always used. `file` is the SECOND level a model download adds
/// (`dev-docs/flux2_model_download.md` §4): optional on the wire, `None` here for every
/// operation that does not publish it, so a consumer that predates it is unaffected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2ProgressFrame {
    pub(super) phase: String,
    /// The OVERALL counter — steps for a generation, bytes across the whole plan for a
    /// download. Deliberately the old field, so every single-level consumer stays right.
    pub(super) step: u64,
    pub(super) total: u64,
    pub(super) label: String,
    pub(super) file: Option<Flux2FileProgress>,
}

/// Reads one progress frame's header.
///
/// Every field is optional and degrades rather than failing: an absent `phase` reads as
/// `generate` (what a generation sends most of the time), absent counters as `0`, and an
/// absent `file_step`/`file_total` pair as "this frame describes no single file". A frame
/// that carries EITHER of the two is taken as describing one, because a backend that
/// reports a byte count without a size still has a file in flight to name.
pub(super) fn parse_flux2_progress_frame(header: &Value) -> Flux2ProgressFrame {
    let u64_field = |name: &str| header.get(name).and_then(Value::as_u64);
    let str_field = |name: &str| header.get(name).and_then(Value::as_str);
    let file_step = u64_field("file_step");
    let file_total = u64_field("file_total");
    let file = if file_step.is_none() && file_total.is_none() {
        None
    } else {
        Some(Flux2FileProgress {
            step: file_step.unwrap_or(0),
            total: file_total.unwrap_or(0),
            label: str_field("file_label").unwrap_or_default().to_string(),
        })
    };
    Flux2ProgressFrame {
        phase: str_field("phase").unwrap_or("generate").to_string(),
        step: u64_field("step").unwrap_or(0),
        total: u64_field("total").unwrap_or(0),
        label: str_field("label").unwrap_or_default().to_string(),
        file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_flags_parse_only_when_complete() {
        let full = json!({
            "image_len": 4,
            "oom_recovered": true,
            "applied": {
                "unload_transformer_before_vae": true,
                "vae_tiling": true,
                "vae_slicing": false,
                "unload_text_encoder_after_encode": true,
                "text_encoder_fp8": false
            }
        });
        let parsed = parse_applied_flags(&full).expect("complete applied object");
        assert!(parsed.unload_transformer_before_vae);
        assert!(parsed.vae_tiling);
        assert!(!parsed.vae_slicing);
        assert!(parsed.unload_text_encoder_after_encode);
        assert!(!parsed.text_encoder_fp8);
        // A backend that reports no `applied` (or a partial one) must leave the user's
        // own settings alone rather than half-overwrite them.
        assert!(parse_applied_flags(&json!({ "image_len": 4 })).is_none());
        assert!(
            parse_applied_flags(&json!({ "applied": { "vae_tiling": true } })).is_none(),
            "a partial applied object is not applied at all"
        );
        // The three OLD flags alone are a partial object now that there are five: an
        // answer that reports nothing about the text encoder must not be half-applied.
        assert!(
            parse_applied_flags(&json!({
                "applied": {
                    "unload_transformer_before_vae": true,
                    "vae_tiling": true,
                    "vae_slicing": false
                }
            }))
            .is_none(),
            "an `applied` object missing the text-encoder flags is not applied at all"
        );
    }

    #[test]
    fn a_stale_run_cannot_touch_the_progress_of_the_next_one() {
        let progress = Mutex::new(Flux2Progress::default());
        let first = begin_progress_generation(&progress);
        update_progress(&progress, first, |state| {
            state.cancel_id = Some(7);
            state.step = 3;
        });
        // Cancel: the id is handed out so the backend can be stopped, and the bar goes.
        assert_eq!(retire_progress_generation(&progress), Some(7));
        assert!(!lock_progress(&progress).active);

        let second = begin_progress_generation(&progress);
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

    #[test]
    fn gib_formatting_is_one_decimal() {
        assert_eq!(format_gib(0), "0.0");
        assert_eq!(format_gib(1024 * 1024 * 1024), "1.0");
    }

    /// The progress split of D6, on the exact frame shapes §4 pins.
    #[test]
    fn a_frame_without_the_file_fields_renders_the_overall_bar_alone() {
        // A preparation frame: the four old fields and nothing else.
        let prepare = parse_flux2_progress_frame(&json!({
            "phase": "download", "step": 0, "total": 40_000_000_000u64, "label": "resolving the file list"
        }));
        assert_eq!(prepare.file, None, "no second level was reported");
        let mut progress = Flux2Progress::default();
        publish_frame_into(&mut progress, prepare);
        let (overall, file) = flux2_progress_fractions(&progress);
        assert!((overall - 0.0).abs() < f32::EPSILON);
        assert_eq!(file, None, "a missing second level draws no second bar");

        // A transfer frame: all six counters.
        let transferring = parse_flux2_progress_frame(&json!({
            "phase": "download",
            "step": 12_884_901_888u64, "total": 40_000_000_000u64,
            "label": "transformer/model-00001-of-00002.safetensors",
            "file_step": 3_221_225_472u64, "file_total": 9_800_000_000u64,
            "file_label": "model-00001-of-00002.safetensors"
        }));
        assert_eq!(
            transferring.file,
            Some(Flux2FileProgress {
                step: 3_221_225_472,
                total: 9_800_000_000,
                label: "model-00001-of-00002.safetensors".to_string(),
            })
        );
        publish_frame_into(&mut progress, transferring);
        let (overall, file) = flux2_progress_fractions(&progress);
        assert!((overall - 0.322_1).abs() < 0.001, "overall was {overall}");
        let file = file.expect("all six counters must draw both bars");
        assert!((file - 0.328_7).abs() < 0.001, "file was {file}");

        // A file whose size the backend could not state: an indeterminate bar, never a
        // division by zero.
        let unsized_file = parse_flux2_progress_frame(&json!({
            "phase": "download", "step": 1, "total": 2,
            "file_step": 512u64, "file_label": "config.json"
        }));
        publish_frame_into(&mut progress, unsized_file);
        let (overall, file) = flux2_progress_fractions(&progress);
        assert!((overall - 0.5).abs() < f32::EPSILON);
        assert_eq!(file, Some(0.0), "an unknown file size is 0.0, not a panic");

        // And a frame that goes back to having no file clears the second bar rather than
        // leaving the previous file on screen.
        publish_frame_into(
            &mut progress,
            parse_flux2_progress_frame(&json!({ "phase": "download", "step": 2, "total": 2 })),
        );
        assert_eq!(flux2_progress_fractions(&progress).1, None);
    }

    /// Applies a frame to a progress state the way [`publish_progress_frame`] does, but
    /// without the generation guard — the guard is already pinned by
    /// `a_stale_run_cannot_touch_the_progress_of_the_next_one`.
    fn publish_frame_into(progress: &mut Flux2Progress, frame: Flux2ProgressFrame) {
        progress.phase = frame.phase;
        progress.step = frame.step;
        progress.total = frame.total;
        progress.label = frame.label;
        progress.file = frame.file;
    }

    // -----------------------------------------------------------------------------------
    // Transfer rate and remaining time
    // -----------------------------------------------------------------------------------

    /// Feeds a scripted frame sequence into a progress state exactly as
    /// [`publish_progress_frame`] does, at `origin + offset`.
    ///
    /// `Instant` cannot be constructed at an arbitrary value, but the estimator only ever
    /// looks at DIFFERENCES, so offsets from one origin script it deterministically.
    fn feed_download(progress: &mut Flux2Progress, origin: Instant, samples: &[(u64, u64)]) {
        for &(millis, step) in samples {
            progress.rate.observe(origin + Duration::from_millis(millis), step);
            progress.step = step;
        }
    }

    const MIB: u64 = 1024 * 1024;

    /// A steady transfer must report the rate it is actually running at, and an estimate
    /// derived from it.
    #[test]
    fn a_scripted_transfer_reports_the_expected_speed_and_estimate() {
        let origin = Instant::now();
        let mut progress = Flux2Progress {
            total: 100 * MIB,
            ..Flux2Progress::default()
        };
        // Exactly 20 MiB/s: twenty frames, 100 ms apart, 2 MiB each. The sequence spans
        // 2 s deliberately — one second of history is BELOW `FLUX2_RATE_MIN_SPAN` and
        // correctly yields no rate at all, which the "too little history" test pins.
        let samples: Vec<(u64, u64)> = (0..=20).map(|i| (i * 100, i * 2 * MIB)).collect();
        feed_download(&mut progress, origin, &samples);
        assert!(Duration::from_millis(2000) >= FLUX2_RATE_MIN_SPAN);

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
            tf!("cleaning.tools.flux2_klein.download.eta_seconds", seconds = 3)
        );

        // The line exists and names both halves.
        assert!(flux2_transfer_status(&progress).is_some());

        // The coarse formatter's three bands.
        assert_eq!(
            format_eta(Duration::from_secs(45)),
            tf!("cleaning.tools.flux2_klein.download.eta_seconds", seconds = 45)
        );
        assert_eq!(
            format_eta(Duration::from_secs(15 * 60)),
            tf!("cleaning.tools.flux2_klein.download.eta_minutes", minutes = 15)
        );
        assert_eq!(
            format_eta(Duration::from_secs(2 * 3600 + 15 * 60)),
            tf!("cleaning.tools.flux2_klein.download.eta_hours", hours = 2, minutes = 15)
        );
    }

    /// At the start of a transfer there is not enough history for any number to mean
    /// something, and the honest answer is silence — not "∞", not "0 с".
    #[test]
    fn too_little_history_yields_no_speed_and_no_estimate() {
        let origin = Instant::now();
        let mut progress = Flux2Progress {
            total: 100 * MIB,
            ..Flux2Progress::default()
        };

        // Nothing at all.
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(progress.rate.remaining(0, progress.total), None);
        assert_eq!(flux2_transfer_status(&progress), None);

        // A single frame is not a rate.
        feed_download(&mut progress, origin, &[(0, 4 * MIB)]);
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(flux2_transfer_status(&progress), None);

        // Two frames 100 ms apart are a burst, not a measurement: the window is shorter
        // than the minimum span, so still nothing.
        feed_download(&mut progress, origin, &[(100, 8 * MIB)]);
        assert!(Duration::from_millis(100) < FLUX2_RATE_MIN_SPAN);
        assert_eq!(progress.rate.bytes_per_second(), None);
        assert_eq!(flux2_transfer_status(&progress), None);

        // Once the window covers the minimum span, a rate appears.
        feed_download(&mut progress, origin, &[(2000, 40 * MIB)]);
        assert!(progress.rate.bytes_per_second().is_some());

        // A counter that does not move is not a rate of zero: it reports nothing, so no
        // estimate can be computed from it and none is shown.
        let mut stalled = Flux2Progress {
            total: 100 * MIB,
            ..Flux2Progress::default()
        };
        feed_download(&mut stalled, origin, &[(0, 8 * MIB), (2000, 8 * MIB), (4000, 8 * MIB)]);
        assert_eq!(stalled.rate.bytes_per_second(), None);
        assert_eq!(stalled.rate.remaining(8 * MIB, 100 * MIB), None);

        // A finished transfer has no remaining time either — never "0 с" beside a full bar.
        let mut done = Flux2Progress {
            total: 40 * MIB,
            ..Flux2Progress::default()
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
        let mut progress = Flux2Progress {
            total: 100 * MIB,
            ..Flux2Progress::default()
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
        assert_eq!(flux2_transfer_status(&progress), None);

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
        let mut restarted = Flux2Progress::default();
        feed_download(&mut restarted, origin, &[(0, 60 * MIB), (2000, 80 * MIB), (2100, 0)]);
        assert_eq!(restarted.rate.bytes_per_second(), None);
        assert_eq!(restarted.rate.remaining(0, 100 * MIB), None);
    }

    /// The optional per-file fields are exactly that: a preparation frame carrying only the
    /// overall level must still drive the speed.
    #[test]
    fn a_frame_without_the_file_fields_still_advances_the_speed() {
        let origin = Instant::now();
        let progress = Arc::new(Mutex::new(Flux2Progress::default()));
        let generation = begin_progress_generation(&progress);

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
            let frame = parse_flux2_progress_frame(&header);
            // Fed through the estimator the same way `publish_progress_frame` does, with a
            // scripted clock instead of the wall clock.
            let mut guard = lock_progress(&progress);
            guard.rate.observe(origin + Duration::from_millis(millis), frame.step);
            guard.step = frame.step;
            guard.total = frame.total;
            guard.file = frame.file;
            drop(guard);
        }
        assert_eq!(generation, lock_progress(&progress).generation);

        let guard = lock_progress(&progress);
        // The last frame carried no file fields, so there is no second bar...
        assert_eq!(guard.file, None);
        assert_eq!(flux2_progress_fractions(&guard).1, None);
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
        assert!(flux2_transfer_status(&guard).is_some());
    }

    /// The numbers belong to ONE transfer and must not outlive it.
    #[test]
    fn a_new_operation_does_not_inherit_the_previous_transfer_s_speed() {
        let origin = Instant::now();
        let progress = Arc::new(Mutex::new(Flux2Progress::default()));
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
        assert_eq!(flux2_transfer_status(&guard), None);
        drop(guard);

        // A retired bar is inactive, which is what stops the line being drawn at all; the
        // renderer returns before reaching it.
        assert!(retire_progress_generation(&progress).is_none());
        assert!(!lock_progress(&progress).active);
    }
}
