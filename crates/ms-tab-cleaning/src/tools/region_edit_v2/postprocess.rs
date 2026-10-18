/*
File: region_edit_v2/postprocess.rs

Purpose:
The host's half of a RAW engine answer (`EnginePoll::DoneRaw`): what a run keeps for it, and the
one latest-wins worker that turns the raw model output into the shown result — again whenever
the engine's live post-process settings change while the result is pending. The pixels are
computed by `ms_region_postprocess::RegionPostprocessor`; this file owns only the inputs, the
thread and the generation bookkeeping, so `host.rs` gains hooks and no state machine.

Key structures:
- `RunCapture`: the unmarked region (and, in overlay-marks mode, the marked region the model
  was sent), kept from the hand-off to the answer
- `PendingRaw`: the worker of one pending raw result, its requested and shown generations
- `PostprocessFailure`: why a derivation produced no image (technical detail for the log)

- `Derived`: one shown image plus the patch of it that can differ from the previous one

Key functions:
- `PendingRaw::start()`: validates the sizes, spawns the worker, submits generation 1
- `PendingRaw::request()`, `poll()`, `settling()`, `lineage()`: the per-frame drive

Notes:
Nothing here blocks the GUI thread: the RGB conversion of every buffer, every render and the
display image (`RegionPostprocessor::render_masked_into` writes the result straight into a copy
of the original's `Color32` pixels) run on the worker; the GUI side only sends a settings value
and drains a channel. The worker wakes the GUI (`request_repaint_of` on the viewport captured at
start) after every answer and when it exits, so a landed derivation is shown on the next frame
without polling. The worker owns one `RegionPostprocessor`, so its stage caches survive between
generations (a feather change costs one alpha + composite, a strength change one remap).
Latest-wins: the worker drains its queue to the newest job before each render, and `poll` drops
every answer (error included) of a generation older than the newest one requested, so a
superseded derivation that lands late is never shown. Dropping a `PendingRaw` drops the job
sender; the worker finishes the render in hand (bounded, a few hundred ms on a 6 MP region at
the finest scale) and exits — it is never joined on the GUI thread.
*/

use super::engine::RawRegionResult;
use super::layers::ResultPatch;
use ms_region_postprocess::{ColourReference, EffectiveSettings, PostprocessSettings, RegionInputs, RegionPostprocessor};
use eframe::egui;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

/// What a run whose engine answers raw pixels keeps from the hand-off until its answer lands.
///
/// The composite BASE is always the unmarked region, so outside the run mask the result is the
/// page as loaded — the marks never reach it, whatever the engine does. The colour-match
/// REFERENCE is the region the model was SENT: with marks composited into it (overlay mode) the
/// ring statistics must come from the image the model actually round-tripped.
#[derive(Debug)]
pub(super) struct RunCapture {
    /// The region as loaded, without the marks.
    original: egui::ColorImage,
    /// The region exactly as the engine received it, when it differs from `original`
    /// (overlay-marks mode with marks drawn).
    sent: Option<egui::ColorImage>,
}

impl RunCapture {
    /// Keeps `original` (unmarked) and, when the model got a different picture, `sent`.
    #[must_use]
    pub(super) fn new(original: egui::ColorImage, sent: Option<egui::ColorImage>) -> Self {
        Self { original, sent }
    }
}

/// Why a derivation produced no image. The text is technical (English `Display` of a typed
/// error, or a thread failure) and goes to the log only; the user reads the host's localized
/// sentence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum PostprocessFailure {
    /// The raw answer and the kept region disagree about the shape.
    #[error("the raw answer does not match the run: {0}")]
    Shape(String),
    /// The worker thread could not be started.
    #[error("the post-process worker could not be started: {0}")]
    Spawn(String),
    /// The post-process refused its inputs or its settings.
    #[error("generation {generation}: {reason}")]
    Render { generation: u64, reason: String },
    /// The worker is gone (it panicked) while a derivation was awaited.
    #[error("the post-process worker stopped with generation {generation} outstanding")]
    WorkerGone { generation: u64 },
}

/// One derivation the GUI asked for.
#[derive(Debug, Clone, Copy)]
struct Job {
    generation: u64,
    settings: EffectiveSettings,
}

/// One derivation the worker finished: the shown image, or why there is none.
struct Answer {
    generation: u64,
    result: Result<Derived, PostprocessFailure>,
}

/// One finished derivation as the GUI shows it.
#[derive(Debug)]
pub(super) struct Derived {
    /// The whole shown image: the rendered pixels inside the run mask (opaque), the original
    /// verbatim outside it.
    pub(super) image: egui::ColorImage,
    /// The bounding box of the run mask cut out of `image`: every derivation of one
    /// `PendingRaw` is the original outside the mask, so two of them differ only inside it.
    /// `None` when the box is the whole region (or the mask is empty): nothing to save.
    pub(super) patch: Option<ResultPatch>,
}

/// The worker of ONE pending raw result and the generation bookkeeping around it.
///
/// A generation is one requested set of effective settings; `requested` is the newest one asked
/// for, `shown` the newest one that came back. The pending result is current exactly when the
/// two agree (`settling`).
#[derive(Debug)]
pub(super) struct PendingRaw {
    /// `None` once the worker could not take a job any more (its receiver is gone).
    job_tx: Option<Sender<Job>>,
    answer_rx: Receiver<Answer>,
    /// The raw settings last asked for, compared against the engine's answer every frame.
    requested_settings: PostprocessSettings,
    /// Their effective form: a change that leaves it equal (a local-strength change while the
    /// colour match is off) needs no new derivation.
    requested_effective: EffectiveSettings,
    requested_generation: u64,
    shown_generation: u64,
    /// A send that failed because the worker is gone, reported by the next `poll`.
    lost: Option<u64>,
    /// The host's id of this pending result: every `Derived` of it shares the outside-mask
    /// pixels, which is what lets `ResultLayer` upload only a patch (`ResultLayer::lineage`).
    lineage: u64,
}

impl PendingRaw {
    /// Validates `raw` against `capture`, spawns ONE `ms_thread` worker owning a
    /// `RegionPostprocessor` (the RGB conversion of every buffer runs ON the worker) and submits
    /// generation 1 for `settings`. `log_tag` prefixes the worker's log lines; `lineage` is the
    /// host's unique id of this pending result (see the field). Call it on the GUI thread: the
    /// worker wakes `ctx`'s CURRENT viewport after every answer.
    ///
    /// # Errors
    /// [`PostprocessFailure::Shape`] when the raw image, the kept regions or a mask do not all
    /// describe the same `w x h` region, [`PostprocessFailure::Spawn`] when the thread cannot be
    /// started. Nothing is left running on an error.
    pub(super) fn start(ctx: &egui::Context, capture: RunCapture, raw: RawRegionResult, settings: PostprocessSettings, log_tag: &'static str, lineage: u64) -> Result<Self, PostprocessFailure> {
        check_shapes(&capture, &raw)?;
        let effective = settings.effective();
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel::<Answer>();
        let waker = Waker { ctx: ctx.clone(), viewport: ctx.viewport_id() };
        ms_thread::Builder::new()
            .name("region-postprocess".to_string())
            .spawn(move || run_worker(capture, raw, &job_rx, &answer_tx, &waker, log_tag))
            .map_err(|error| PostprocessFailure::Spawn(error.to_string()))?;
        let mut pending = Self {
            job_tx: Some(job_tx),
            answer_rx,
            requested_settings: settings,
            requested_effective: effective,
            requested_generation: 0,
            shown_generation: 0,
            lost: None,
            lineage,
        };
        pending.submit(effective);
        Ok(pending)
    }

    /// The host's id of this pending result (`start`).
    #[must_use]
    pub(super) fn lineage(&self) -> u64 {
        self.lineage
    }

    /// Asks for a derivation with `settings` unless they are the ones last asked for (or have
    /// the same effective form). Cheap; called every frame with the engine's live settings.
    pub(super) fn request(&mut self, settings: PostprocessSettings) {
        if settings == self.requested_settings {
            return;
        }
        self.requested_settings = settings;
        let effective = settings.effective();
        if effective == self.requested_effective {
            return;
        }
        self.requested_effective = effective;
        self.submit(effective);
    }

    /// Sends one job under the next generation; a dead worker is remembered for `poll`.
    fn submit(&mut self, settings: EffectiveSettings) {
        self.requested_generation = self.requested_generation.saturating_add(1);
        let generation = self.requested_generation;
        let sent = self.job_tx.as_ref().is_some_and(|tx| tx.send(Job { generation, settings }).is_ok());
        if !sent {
            self.job_tx = None;
            self.lost = Some(generation);
        }
    }

    /// The answer of the newest REQUESTED generation once it arrived, or `None` while it is
    /// still awaited (or was already returned). Every answer of an older generation is dropped,
    /// its error included (latest-wins): a superseded image is never shown, even when it lands
    /// before the one the live settings describe. A worker that is gone while the requested
    /// derivation is still awaited is reported as [`PostprocessFailure::WorkerGone`], once.
    pub(super) fn poll(&mut self) -> Option<Result<Derived, PostprocessFailure>> {
        let mut newest: Option<Answer> = None;
        let mut disconnected = false;
        loop {
            match self.answer_rx.try_recv() {
                Ok(answer) => {
                    // Only the requested generation describes the live settings; an older one
                    // is obsolete even though it is newer than the one shown.
                    if answer.generation == self.requested_generation && self.shown_generation != self.requested_generation {
                        newest = Some(answer);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some(answer) = newest {
            self.shown_generation = answer.generation;
            return Some(answer.result);
        }
        let outstanding = self.settling().then_some(self.requested_generation);
        let gone = self.lost.take().or(if disconnected { outstanding } else { None });
        gone.map(|generation| {
            // Nothing will ever answer it: stop reporting it as in flight.
            self.shown_generation = self.requested_generation;
            Err(PostprocessFailure::WorkerGone { generation })
        })
    }

    /// Whether a derivation is still in flight: the newest one asked for has not come back.
    #[must_use]
    pub(super) fn settling(&self) -> bool {
        self.shown_generation != self.requested_generation
    }
}

/// Every buffer of `raw` and `capture` must describe the region of `raw.image`.
fn check_shapes(capture: &RunCapture, raw: &RawRegionResult) -> Result<(), PostprocessFailure> {
    let [w, h] = raw.image.size;
    let area = w.checked_mul(h).ok_or_else(|| PostprocessFailure::Shape(format!("{w}x{h} overflows")))?;
    if capture.original.size != raw.image.size {
        return Err(PostprocessFailure::Shape(format!("the answer is {w}x{h}, the loaded region {}x{}", capture.original.size[0], capture.original.size[1])));
    }
    if let Some(sent) = &capture.sent
        && sent.size != raw.image.size
    {
        return Err(PostprocessFailure::Shape(format!("the answer is {w}x{h}, the sent region {}x{}", sent.size[0], sent.size[1])));
    }
    if raw.mask.len() != area {
        return Err(PostprocessFailure::Shape(format!("the run mask holds {} bytes, a {w}x{h} region needs {area}", raw.mask.len())));
    }
    if let Some(sample) = &raw.sample_mask
        && sample.len() != area
    {
        return Err(PostprocessFailure::Shape(format!("the sample mask holds {} bytes, a {w}x{h} region needs {area}", sample.len())));
    }
    Ok(())
}

/// The RGB8 bytes of `image`, row-major; alpha dropped after un-premultiplying, the same
/// conversion the region takes on its way to the backend (`ms_tools::png_wire`).
fn rgb_bytes(image: &egui::ColorImage) -> Vec<u8> {
    image.pixels.iter().flat_map(|pixel| {
        let [r, g, b, _] = pixel.to_srgba_unmultiplied();
        [r, g, b]
    }).collect()
}

/// Wakes the GUI from the worker: `egui::Context` is `Send + Sync`, and `request_repaint_of`
/// from another thread wakes an eframe event loop.
#[derive(Debug)]
struct Waker {
    ctx: egui::Context,
    /// The viewport the host draws in, captured on the GUI thread at `start` (off the GUI
    /// thread `Context::viewport_id` names no particular viewport).
    viewport: egui::ViewportId,
}

impl Waker {
    fn wake(&self) {
        self.ctx.request_repaint_of(self.viewport);
    }
}

impl Drop for Waker {
    /// The worker's last word, also when it unwinds from a panic: the GUI's next `poll` then
    /// sees the closed channel and reports an outstanding generation as lost.
    fn drop(&mut self) {
        self.wake();
    }
}

/// `(x0, y0, x1, y1)` (exclusive end) of the nonzero pixels of a `w`-wide mask, `None` when
/// it is empty.
fn mask_bbox(mask: &[u8], w: usize) -> Option<[usize; 4]> {
    let mut bbox: Option<[usize; 4]> = None;
    for (y, row) in mask.chunks_exact(w.max(1)).enumerate() {
        let (Some(first), Some(last)) = (row.iter().position(|&m| m != 0), row.iter().rposition(|&m| m != 0)) else {
            continue;
        };
        let b = bbox.get_or_insert([first, y, last + 1, y + 1]);
        b[0] = b[0].min(first);
        b[2] = b[2].max(last + 1);
        b[3] = y + 1;
    }
    bbox
}

/// The pixels of `image` inside `bbox` as a patch, `None` when `bbox` is the whole image.
fn cut_patch(image: &egui::ColorImage, bbox: [usize; 4]) -> Option<ResultPatch> {
    let [x0, y0, x1, y1] = bbox;
    let [w, h] = image.size;
    if [x0, y0, x1, y1] == [0, 0, w, h] {
        return None;
    }
    let mut pixels = Vec::with_capacity((x1 - x0) * (y1 - y0));
    for row in image.pixels.chunks_exact(w).skip(y0).take(y1 - y0) {
        pixels.extend_from_slice(&row[x0..x1]);
    }
    Some(ResultPatch { pos: [x0, y0], image: egui::ColorImage::new([x1 - x0, y1 - y0], pixels) })
}

/// Renders one derivation: the original's pixels with the result written inside the mask
/// (opaque), outside it the ORIGINAL pixel verbatim — alpha included, which the RGB render
/// cannot carry — in one row-parallel pass, plus the patch the GUI may upload instead.
fn derive(processor: &mut RegionPostprocessor, original: &egui::ColorImage, bbox: Option<[usize; 4]>, settings: &EffectiveSettings) -> Result<Derived, ms_region_postprocess::PostprocessError> {
    let mut pixels = original.pixels.clone();
    processor.render_masked_into(settings, &mut pixels, |px, [r, g, b]| *px = egui::Color32::from_rgb(r, g, b))?;
    let image = egui::ColorImage::new(original.size, pixels);
    let patch = bbox.and_then(|bbox| cut_patch(&image, bbox));
    Ok(Derived { image, patch })
}

/// The worker body: builds the post-processor once, then renders the NEWEST job of every batch
/// until the job sender is dropped. Every outcome is answered and wakes the GUI; a closed
/// answer channel (the host dropped the result) ends the loop. The first derivation is logged
/// at INFO (size, settings, duration), later ones only to the trace log; failures warn.
fn run_worker(capture: RunCapture, raw: RawRegionResult, job_rx: &Receiver<Job>, answer_tx: &Sender<Answer>, waker: &Waker, log_tag: &'static str) {
    let [w, h] = raw.image.size;
    let inputs = RegionInputs {
        width: w,
        height: h,
        base: rgb_bytes(&capture.original),
        reference: capture.sent.as_ref().map(rgb_bytes),
        generated: rgb_bytes(&raw.image),
        mask: raw.mask.clone(),
        sample_mask: raw.sample_mask,
        // A whole-region run has no ring outside its (solid) mask: it is colour-matched against
        // the region's edge band instead.
        colour_reference: if raw.whole_region { ColourReference::EdgeBand } else { ColourReference::Ring },
    };
    let bbox = mask_bbox(&raw.mask, w);
    let mut processor = RegionPostprocessor::new(inputs);
    let mut first = true;
    while let Ok(mut job) = job_rx.recv() {
        // Latest-wins: everything queued behind this job is newer and supersedes it.
        while let Ok(newer) = job_rx.try_recv() {
            job = newer;
        }
        let started = web_time::Instant::now();
        ms_log::trace_log!(ms_log::trace::cat::RENDER, "{log_tag} post-process generation {} of a {w}x{h} region started: {:?}", job.generation, job.settings);
        let result = match processor.as_mut() {
            Ok(processor) => derive(processor, &capture.original, bbox, &job.settings).map_err(|error| PostprocessFailure::Render { generation: job.generation, reason: error.to_string() }),
            Err(error) => Err(PostprocessFailure::Render { generation: job.generation, reason: error.to_string() }),
        };
        let elapsed_ms = started.elapsed().as_millis();
        match &result {
            Ok(_) if first => ms_log::runtime_log::log_info(format!(
                "{log_tag} post-process of a {w}x{h} region: first derivation (generation {}) in {elapsed_ms} ms, {:?}",
                job.generation, job.settings
            )),
            Ok(_) => ms_log::trace_log!(ms_log::trace::cat::RENDER, "{log_tag} post-process generation {} finished in {elapsed_ms} ms", job.generation),
            Err(error) => ms_log::runtime_log::log_warn(format!("{log_tag} post-process failed after {elapsed_ms} ms: {error}")),
        }
        first = false;
        let sent = answer_tx.send(Answer { generation: job.generation, result }).is_ok();
        waker.wake();
        if !sent {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Color32;

    /// A 4x4 grey region with a 2x2 mask in its centre, and a red raw answer.
    fn fixture() -> (RunCapture, RawRegionResult) {
        let original = egui::ColorImage::filled([4, 4], Color32::from_rgb(90, 90, 90));
        let mut mask = vec![0u8; 16];
        for idx in [5, 6, 9, 10] {
            mask[idx] = 255;
        }
        let raw = RawRegionResult { image: egui::ColorImage::filled([4, 4], Color32::from_rgb(200, 0, 0)), mask, sample_mask: None, whole_region: false };
        (RunCapture::new(original, None), raw)
    }

    /// Starts a pending result on a context that has no event loop (repaint requests are
    /// recorded and dropped).
    fn start(capture: RunCapture, raw: RawRegionResult, settings: PostprocessSettings) -> Result<PendingRaw, PostprocessFailure> {
        PendingRaw::start(&egui::Context::default(), capture, raw, settings, "[test]", 7)
    }

    /// Polls until a derivation lands or the deadline passes.
    fn wait(pending: &mut PendingRaw) -> Result<egui::ColorImage, PostprocessFailure> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(result) = pending.poll() {
                return result.map(|derived| derived.image);
            }
            assert!(std::time::Instant::now() < deadline, "the worker never answered");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Generation 1 lands, outside the mask the original pixel survives, and a new setting
    /// starts generation 2 (settling until it lands); an unchanged setting starts nothing.
    #[test]
    fn a_settings_change_re_derives_the_result() {
        let (capture, raw) = fixture();
        let settings = PostprocessSettings { mask_feather_px: 0, ..PostprocessSettings::default() };
        let mut pending = start(capture, raw, settings).expect("the shapes agree");
        assert!(pending.settling());
        let first = wait(&mut pending).expect("generation 1 renders");
        assert!(!pending.settling());
        assert_eq!(first.pixels[0], Color32::from_rgb(90, 90, 90), "outside the mask: the original");
        assert_eq!(first.pixels[5], Color32::from_rgb(200, 0, 0), "a hard edge keeps the model pixel");

        pending.request(settings);
        assert!(!pending.settling(), "the same settings start nothing");
        pending.request(PostprocessSettings { mask_feather_px: 2, ..settings });
        assert!(pending.settling());
        let second = wait(&mut pending).expect("generation 2 renders");
        assert!(!pending.settling());
        assert_eq!(second.pixels[0], first.pixels[0]);
    }

    /// A whole-region run is colour-matched too (against its edge band), so toggling the
    /// colour match re-derives the result.
    #[test]
    fn a_colour_toggle_in_whole_region_mode_re_derives() {
        let (capture, mut raw) = fixture();
        raw.whole_region = true;
        let settings = PostprocessSettings::default();
        let mut pending = start(capture, raw, settings).expect("the shapes agree");
        wait(&mut pending).expect("generation 1 renders");
        pending.request(PostprocessSettings { color_match: !settings.color_match, ..settings });
        assert!(pending.settling(), "the toggle changes what is applied");
        wait(&mut pending).expect("generation 2 renders");
    }

    /// A change that leaves the effective settings equal (the local strength while the colour
    /// match is off) needs no new run.
    #[test]
    fn an_effectively_equal_change_starts_nothing() {
        let (capture, raw) = fixture();
        let settings = PostprocessSettings { color_match: false, ..PostprocessSettings::default() };
        let mut pending = start(capture, raw, settings).expect("the shapes agree");
        wait(&mut pending).expect("generation 1 renders");
        pending.request(PostprocessSettings { color_match_local_pct: settings.color_match_local_pct + 10, ..settings });
        assert!(!pending.settling());
    }

    /// Only the newest answer is shown: older ones queued behind it are dropped.
    #[test]
    fn older_answers_are_dropped() {
        let (capture, raw) = fixture();
        let settings = PostprocessSettings::default();
        let mut pending = start(capture, raw, settings).expect("the shapes agree");
        for feather in 1..=5 {
            pending.request(PostprocessSettings { mask_feather_px: feather, ..settings });
        }
        while pending.settling() {
            wait(&mut pending).expect("every generation renders");
        }
        assert_eq!(pending.shown_generation, pending.requested_generation);
        assert!(pending.poll().is_none(), "nothing newer is left");
    }

    /// A `PendingRaw` without a worker: the test plays the worker through the returned channel
    /// ends, so the order in which answers land is fixed.
    fn scripted(settings: PostprocessSettings) -> (PendingRaw, Receiver<Job>, Sender<Answer>) {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel::<Answer>();
        let effective = settings.effective();
        let mut pending = PendingRaw {
            job_tx: Some(job_tx),
            answer_rx,
            requested_settings: settings,
            requested_effective: effective,
            requested_generation: 0,
            shown_generation: 0,
            lost: None,
            lineage: 1,
        };
        pending.submit(effective);
        (pending, job_rx, answer_tx)
    }

    /// Generation 1 finishing AFTER generation 2 was requested is obsolete: neither its image
    /// nor its error is shown, the result keeps settling, and generation 2 is shown once it lands.
    #[test]
    fn a_superseded_generation_that_lands_late_is_never_shown() {
        let settings = PostprocessSettings { mask_feather_px: 0, ..PostprocessSettings::default() };
        let (mut pending, job_rx, answer_tx) = scripted(settings);
        pending.request(PostprocessSettings { mask_feather_px: 3, ..settings });
        let generations: Vec<u64> = job_rx.try_iter().map(|job| job.generation).collect();
        assert_eq!(generations, vec![1, 2]);

        let stale = egui::ColorImage::filled([1, 1], Color32::RED);
        answer_tx.send(Answer { generation: 1, result: Ok(Derived { image: stale, patch: None }) }).expect("the receiver is alive");
        assert!(pending.poll().is_none(), "generation 1 is superseded");
        assert!(pending.settling(), "generation 2 is still awaited");
        answer_tx.send(Answer { generation: 1, result: Err(PostprocessFailure::Render { generation: 1, reason: "stale".to_string() }) }).expect("the receiver is alive");
        assert!(pending.poll().is_none(), "a superseded error is dropped too");
        assert!(pending.settling());

        let current = egui::ColorImage::filled([1, 1], Color32::GREEN);
        answer_tx.send(Answer { generation: 2, result: Ok(Derived { image: current.clone(), patch: None }) }).expect("the receiver is alive");
        assert_eq!(pending.poll().map(|result| result.map(|derived| derived.image.pixels)), Some(Ok(current.pixels)));
        assert!(!pending.settling());
        assert!(pending.poll().is_none(), "the requested generation is returned once");
    }

    /// The worker vanishing while only a superseded answer came back still reports the
    /// requested generation as lost.
    #[test]
    fn a_worker_gone_after_a_superseded_answer_reports_the_requested_generation() {
        let settings = PostprocessSettings::default();
        let (mut pending, job_rx, answer_tx) = scripted(settings);
        pending.request(PostprocessSettings { mask_feather_px: settings.mask_feather_px + 1, ..settings });
        answer_tx.send(Answer { generation: 1, result: Ok(Derived { image: egui::ColorImage::filled([1, 1], Color32::RED), patch: None }) }).expect("the receiver is alive");
        drop(answer_tx);
        drop(job_rx);
        assert_eq!(pending.poll().map(|result| result.map(|derived| derived.image.size)), Some(Err(PostprocessFailure::WorkerGone { generation: 2 })));
        assert!(!pending.settling());
    }

    /// Every buffer must describe the answer's region; a mismatch is refused before a thread
    /// starts.
    #[test]
    fn mismatched_shapes_are_refused() {
        let (capture, mut raw) = fixture();
        raw.mask.pop();
        assert!(matches!(start(capture, raw, PostprocessSettings::default()), Err(PostprocessFailure::Shape(_))));
        let (_, raw) = fixture();
        let capture = RunCapture::new(egui::ColorImage::filled([4, 3], Color32::WHITE), None);
        assert!(matches!(start(capture, raw, PostprocessSettings::default()), Err(PostprocessFailure::Shape(_))));
        let (_, mut raw) = fixture();
        raw.sample_mask = Some(vec![0; 3]);
        let capture = RunCapture::new(egui::ColorImage::filled([4, 4], Color32::WHITE), Some(egui::ColorImage::filled([4, 4], Color32::RED)));
        assert!(matches!(start(capture, raw, PostprocessSettings::default()), Err(PostprocessFailure::Shape(_))));
    }

    /// A translucent original pixel outside the mask keeps its alpha; inside, the result is
    /// opaque; the patch is the mask's bounding box cut out of the shown image.
    #[test]
    fn outside_the_mask_the_original_survives_verbatim() {
        let translucent = Color32::from_rgba_premultiplied(40, 40, 40, 128);
        let original = egui::ColorImage::new([3, 2], vec![translucent, Color32::WHITE, translucent, translucent, translucent, translucent]);
        let mask = vec![0, 255, 0, 0, 0, 0];
        let inputs = RegionInputs { width: 3, height: 2, base: rgb_bytes(&original), reference: None, generated: vec![7; 18], mask: mask.clone(), sample_mask: None, colour_reference: ColourReference::Ring };
        let mut processor = RegionPostprocessor::new(inputs).expect("the shapes agree");
        let settings = PostprocessSettings { mask_feather_px: 0, color_match: false, ..PostprocessSettings::default() }.effective();
        let derived = derive(&mut processor, &original, mask_bbox(&mask, 3), &settings).expect("the render succeeds");
        let mut expected = original.pixels.clone();
        expected[1] = Color32::from_rgb(7, 7, 7);
        assert_eq!(derived.image.pixels, expected);
        let patch = derived.patch.expect("a one-pixel box is not the whole region");
        assert_eq!((patch.pos, patch.image.size, patch.image.pixels), ([1, 0], [1, 1], vec![Color32::from_rgb(7, 7, 7)]));
    }

    /// The bounding box spans every set pixel; an empty mask has none, a full one is the whole
    /// region (and then no patch is cut).
    #[test]
    fn the_mask_bbox_bounds_every_set_pixel() {
        let mask = [0, 0, 0, 0, 0, 0, 9, 0, 0, 255, 0, 0, 0, 0, 0, 0];
        assert_eq!(mask_bbox(&mask, 4), Some([1, 1, 3, 3]));
        assert_eq!(mask_bbox(&[0; 6], 3), None);
        assert_eq!(mask_bbox(&[1; 6], 3), Some([0, 0, 3, 2]));
        assert!(cut_patch(&egui::ColorImage::filled([3, 2], Color32::RED), [0, 0, 3, 2]).is_none());
    }
}
