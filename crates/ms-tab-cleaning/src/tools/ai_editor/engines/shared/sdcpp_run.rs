/*
File: cleaning/tools/ai_editor/engines/shared/sdcpp_run.rs

Purpose:
One region-edit run of a large-model engine (FLUX.2 klein, FLUX.3 klein) on the
Stable-Diffusion.cpp runtime: the worker that reads the app-wide sd.cpp choice, checks the
`--no-ai` / platform gate, builds the `GenerateJob` and runs it in the sd.cpp worker process
(`ms_sdcpp::service().generate`), feeding the engine's ONE shared progress bar. Model-agnostic:
the engine supplies the model files and the generation request.

Key items:
- `SdCppGenerate`: the service seam (`generate` / `unload` / `is_worker_running`), implemented
  for `ms_sdcpp::SdCppService`; tests inject a fake.
- `SdCppRunInput`: what an engine hands a run (files, request, its `SdCppRunOptions`).
- `SdCppRunJob`: `start` (claims the progress generation, spawns the worker), `poll`, `cancel`
  (parks the receiver in the shared `AbandonedJobs`, retires the generation, raises the flag).
- `read_sdcpp_choice()`: the blocking `user_config` read of `SdCppChoice` for engine workers.
- `sdcpp_route_available()`: whether sd.cpp can run here at all (route rule + a build offered).
- `rgb_to_color_image()`: the answer's conversion, run on the job's worker.
- `spawn_sdcpp_unload()`: stops the idle worker process on a thread (the «Выгрузить» button).

Notes:
Native-only (`ms-sdcpp` is empty on wasm); `mod.rs` gates the module. Nothing here runs on the
GUI thread except `start` / `poll` / `cancel`, which only spawn, drain a channel or store a
flag. The progress writes go through the shared generation guard (`update_progress`), so a
cancelled run can never move the bar of the run that replaced it. Stage texts are
`cleaning.tools.sdcpp.stage_*`; errors are `SdCppError`'s localized `Display`.
*/

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use eframe::egui;
use ms_config::SdCppChoice;
use ms_local_ai::local_ai::{AiRouteKind, LocalAiAvailability};
use ms_log::runtime_log;
use ms_sdcpp::{GenerateJob, GenerateRequest, ModelFiles, RgbImage, RunEvent, RunStage, SdCppBuild, SdCppError, SdCppService};
use ms_thread as thread;

use super::sdcpp_options::SdCppRunOptions;
use super::stream_progress::{AbandonedJobs, EngineProgress, begin_progress_generation, retire_progress_generation, update_progress};

/// The sd.cpp service as a run sees it. Implemented for [`SdCppService`] (the process-wide
/// `ms_sdcpp::service()`); a test implements it with a scripted fake.
pub(in crate::tools::ai_editor::engines) trait SdCppGenerate: Send + Sync {
    /// Runs `job` (blocking) and returns the image; see `SdCppService::generate`.
    ///
    /// # Errors
    /// The service's typed [`SdCppError`] (busy, not installed, worker crash, cancelled, …).
    fn generate(&self, job: &GenerateJob, cancel: &AtomicBool, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError>;

    /// Stops the idle worker process (blocking up to the service's shutdown wait).
    fn unload(&self);

    /// Whether a worker process is alive.
    fn is_worker_running(&self) -> bool;
}

impl SdCppGenerate for SdCppService {
    fn generate(&self, job: &GenerateJob, cancel: &AtomicBool, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError> {
        SdCppService::generate(self, job, cancel, events)
    }

    fn unload(&self) {
        SdCppService::unload(self);
    }

    fn is_worker_running(&self) -> bool {
        SdCppService::is_worker_running(self)
    }
}

/// What an engine hands one sd.cpp run.
#[derive(Debug, Clone)]
pub(in crate::tools::ai_editor::engines) struct SdCppRunInput {
    /// The model files (absolute paths), resolved by the engine on its status worker.
    pub(in crate::tools::ai_editor::engines) files: ModelFiles,
    /// The generation; its `vae_tiling` is overwritten from `options`.
    pub(in crate::tools::ai_editor::engines) request: GenerateRequest,
    /// The engine's persisted sd.cpp options (normalized by the run).
    pub(in crate::tools::ai_editor::engines) options: SdCppRunOptions,
}

/// Everything a run reads besides its input: the service, the persisted choice, the process
/// gate and the build resolution. [`Self::process`] is the product; tests inject their own.
struct SdCppRunEnv {
    service: &'static dyn SdCppGenerate,
    read_choice: fn() -> SdCppChoice,
    availability: LocalAiAvailability,
    resolve_build: fn(&SdCppChoice) -> Option<SdCppBuild>,
}

impl SdCppRunEnv {
    /// The process-wide service, `user_config`, the installed `--no-ai` / platform answer and
    /// the catalog's build choice.
    fn process() -> Self {
        Self { service: ms_sdcpp::service(), read_choice: read_sdcpp_choice, availability: ms_local_ai::local_ai::process(), resolve_build: ms_sdcpp::build_from_choice }
    }
}

/// Reads the persisted [`SdCppChoice`] from `user_config`. A missing or unreadable document
/// reads as the default choice with a log line (the setting must never stop a run from being
/// tried; the service then reports what is really wrong).
///
/// # Threading
/// Blocking file I/O: worker threads only (an engine's status worker or a run worker).
#[must_use]
pub(in crate::tools::ai_editor::engines) fn read_sdcpp_choice() -> SdCppChoice {
    match ms_config::load_raw_user_settings_for_startup() {
        Ok(settings) => ms_config::sdcpp_choice_from_user_settings(&settings),
        Err(err) => {
            runtime_log::log_warn(format!("[cleaning] cannot read user_config for the sd.cpp choice; using defaults: {err:#}"));
            SdCppChoice::default()
        }
    }
}

/// Whether this process can run Stable-Diffusion.cpp at all: the ONE route rule
/// (`AiRouteKind::SdCpp.availability`: `--no-ai` and the platform) AND a catalog build offered
/// on this machine (`ms_sdcpp::offers_a_build_here`; today the platform feature is on only where the
/// pinned release ships an archive, so this half guards a future platform without rows). It
/// decides whether the runtime switch is offered, whether a hint may suggest switching to
/// sd.cpp and whether the automatic switch may move there (`sdcpp_auto_switch`); installation
/// is not part of it. Cheap (a process-global read and a static catalog filter): safe every
/// frame.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn sdcpp_route_available() -> bool {
    AiRouteKind::SdCpp.availability(&ms_local_ai::local_ai::process()).is_ok() && ms_sdcpp::offers_a_build_here()
}

/// The host's `ColorImage` of an sd.cpp answer (opaque). Linear in the pixel count: worker
/// threads only (the run job calls it on its worker).
///
/// # Errors
/// A localized size message when a side does not fit `usize` (not on supported targets).
pub(in crate::tools::ai_editor::engines) fn rgb_to_color_image(image: &RgbImage) -> Result<egui::ColorImage, String> {
    let side = |px: u32| usize::try_from(px).map_err(|_| t!("cleaning.inpaint.size_mismatch_error").to_string());
    // `RgbImage` guarantees `pixels.len() == width * height * 3`, exactly the precondition
    // `from_rgb` asserts.
    Ok(egui::ColorImage::from_rgb([side(image.width())?, side(image.height())?], image.pixels()))
}

/// Stops the idle sd.cpp worker process (and frees its model) on a background thread; a running
/// generation shuts its worker down when it ends. Non-blocking: safe on the GUI thread.
pub(in crate::tools::ai_editor::engines) fn spawn_sdcpp_unload() {
    thread::spawn(|| {
        unload_if_running(ms_sdcpp::service());
    });
}

/// Stops `service`'s worker when one runs; returns whether it asked. Blocking (the service's
/// shutdown wait): worker threads only.
fn unload_if_running(service: &dyn SdCppGenerate) -> bool {
    if !service.is_worker_running() {
        runtime_log::log_info("[cleaning] sd.cpp unload requested from the region editor: no worker runs");
        return false;
    }
    runtime_log::log_info("[cleaning] sd.cpp unload requested from the region editor");
    service.unload();
    true
}

/// Raises the run's cancel flag when dropped: dropping the job (host teardown, an engine
/// switch) stops the generation instead of letting it finish for nobody.
#[derive(Debug)]
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One sd.cpp run in flight: its answer channel, its cancel flag (raised on cancel AND on
/// drop) and the progress it claimed.
pub(in crate::tools::ai_editor::engines) struct SdCppRunJob {
    rx: Receiver<Result<egui::ColorImage, String>>,
    progress: Arc<Mutex<EngineProgress>>,
    cancel: CancelOnDrop,
}

impl SdCppRunJob {
    /// Claims `progress` for a new run (on the calling GUI thread, so two runs started in a row
    /// are ordered) and starts the run on a worker: gate, `user_config` choice, job, generate.
    /// `log_label` names the engine in the log.
    pub(in crate::tools::ai_editor::engines) fn start(input: SdCppRunInput, progress: &Arc<Mutex<EngineProgress>>, log_label: &'static str) -> Self {
        Self::start_with(SdCppRunEnv::process(), input, progress, log_label)
    }

    /// [`Self::start`] over an explicit environment.
    fn start_with(env: SdCppRunEnv, input: SdCppRunInput, progress: &Arc<Mutex<EngineProgress>>, log_label: &'static str) -> Self {
        let claim = begin_progress_generation(progress);
        let generation = claim.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker_progress = Arc::clone(progress);
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            // The answer is converted HERE, on the worker: a 4 MP RGB -> `ColorImage` pass is not
            // GUI-thread work.
            let result = run_sdcpp(&env, input, &worker_cancel, &worker_progress, generation, log_label).and_then(|image| rgb_to_color_image(&image));
            // The bar ends with the run; a retired generation's write is dropped by the guard.
            update_progress(&worker_progress, generation, |state| state.active = false);
            // The receiver is gone (or parked in `AbandonedJobs`) when the engine abandoned the
            // run; the answer is then unwanted and dropping it is the intent.
            let _ = tx.send(result);
        });
        Self { rx, progress: Arc::clone(progress), cancel: CancelOnDrop(cancel) }
    }

    /// The run's answer once it arrived: the generated image (opaque, exactly the request's
    /// size — the service refuses any other — already converted on the worker), or a localized
    /// error. `None` while it runs. A worker that died without answering is reported as an
    /// error. Call until it answers once, then drop the job.
    pub(in crate::tools::ai_editor::engines) fn poll(&mut self) -> Option<Result<egui::ColorImage, String>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string())),
        }
    }

    /// Abandons the run: the cancel flag is raised (the service sends `Cancel` to the worker
    /// and kills it after its grace period), the progress generation is retired so the bar
    /// disappears at once, and the receiver is parked in `abandoned` until the worker ends (the
    /// engine refuses a new run while `abandoned.busy()`). `log_label` names the engine.
    pub(in crate::tools::ai_editor::engines) fn cancel(self, abandoned: &mut AbandonedJobs, log_label: &'static str) {
        let Self { rx, progress, cancel } = self;
        cancel.0.store(true, Ordering::SeqCst);
        runtime_log::log_info(format!("[cleaning] {log_label} sd.cpp run cancelled"));
        abandoned.push(rx);
        // The claim's control never received a backend request id, so retiring sends nothing to
        // the Python backend; it only hides the bar and drops the worker's later writes.
        retire_progress_generation(&progress, log_label);
    }
}

/// The worker body: gate, choice, build, job, generate. Errors are user-facing (localized).
fn run_sdcpp(env: &SdCppRunEnv, input: SdCppRunInput, cancel: &AtomicBool, progress: &Mutex<EngineProgress>, generation: u64, log_label: &'static str) -> Result<RgbImage, String> {
    AiRouteKind::SdCpp.availability(&env.availability).map_err(|off| off.user_message())?;
    let choice = (env.read_choice)();
    let Some(build) = (env.resolve_build)(&choice) else {
        runtime_log::log_warn(format!("[cleaning] {log_label} sd.cpp run refused: no sd.cpp build is offered on this machine"));
        return Err(t!("cleaning.tools.sdcpp.readiness_not_installed").to_string());
    };
    let SdCppRunInput { files, mut request, options } = input;
    let options = options.normalized();
    request.vae_tiling = options.vae_tiling;
    let job = GenerateJob { build, files, context: options.to_context(&choice), request };
    runtime_log::log_info(format!(
        "[cleaning] {log_label} sd.cpp run: build '{}', family {:?}, {}x{}, steps {}, references {}, context {:?}",
        build.wire(),
        job.files.family,
        job.request.width,
        job.request.height,
        job.request.steps,
        job.request.references.len(),
        job.context
    ));
    let mut on_event = |event: RunEvent| publish_run_event(progress, generation, event);
    match env.service.generate(&job, cancel, &mut on_event) {
        Ok(image) => {
            runtime_log::log_info(format!("[cleaning] {log_label} sd.cpp run finished: {}x{}", image.width(), image.height()));
            Ok(image)
        }
        Err(SdCppError::GenerationCancelled) => {
            runtime_log::log_info(format!("[cleaning] {log_label} sd.cpp run stopped by cancel"));
            Err(SdCppError::GenerationCancelled.to_string())
        }
        Err(err) => {
            runtime_log::log_error(format!("[cleaning] {log_label} sd.cpp run failed: {err:?}"));
            Err(err.to_string())
        }
    }
}

/// The localized text of a run stage.
fn stage_text(stage: RunStage) -> &'static str {
    match stage {
        RunStage::StartingWorker => t!("cleaning.tools.sdcpp.stage_starting"),
        RunStage::LoadingModel => t!("cleaning.tools.sdcpp.stage_loading"),
        RunStage::Generating => t!("cleaning.tools.sdcpp.stage_generating"),
    }
}

/// Writes one run event into the shared progress under `generation`.
///
/// A stage becomes a counter-less `generate` frame whose `label` is the stage text (the bar
/// shows the label alone); a step becomes the step counter («Шаг x/y»), keeping that label.
/// Neither carries a file level or bytes, so no speed line appears.
fn publish_run_event(progress: &Mutex<EngineProgress>, generation: u64, event: RunEvent) {
    update_progress(progress, generation, |state| {
        state.phase = "generate".to_string();
        state.stage = None;
        state.file = None;
        match event {
            RunEvent::Stage(stage) => {
                state.step = 0;
                state.total = 0;
                state.label = stage_text(stage).to_string();
            }
            RunEvent::Step { step, steps } => {
                state.step = u64::from(step);
                state.total = u64::from(steps);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use ms_local_ai::local_ai::LocalAiAvailability;
    use ms_sdcpp::{DeviceSelection, ModelFamily};

    use super::super::sdcpp_options::FlashAttentionChoice;
    use super::super::stream_progress::lock_progress;
    use super::*;

    /// What the fake does when asked to generate.
    #[derive(Clone, Copy)]
    enum Script {
        /// Emits two stages and two steps, then answers a `width` x `height` image.
        Succeed,
        /// Waits for the cancel flag, then answers `Cancelled`.
        WaitForCancel,
        /// Answers a worker error.
        Fail,
    }

    /// A scripted service recording the jobs it was given.
    struct FakeService {
        script: Script,
        jobs: Mutex<Vec<GenerateJob>>,
        unloads: AtomicUsize,
        running: AtomicBool,
    }

    impl SdCppGenerate for FakeService {
        fn generate(&self, job: &GenerateJob, cancel: &AtomicBool, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError> {
            self.jobs.lock().expect("fake lock").push(job.clone());
            match self.script {
                Script::Succeed => {
                    events(RunEvent::Stage(RunStage::StartingWorker));
                    events(RunEvent::Stage(RunStage::Generating));
                    events(RunEvent::Step { step: 1, steps: 2 });
                    events(RunEvent::Step { step: 2, steps: 2 });
                    let pixels = usize::try_from(job.request.width * job.request.height * 3).expect("tiny test image");
                    Ok(RgbImage::new(job.request.width, job.request.height, vec![7; pixels]).expect("sized buffer"))
                }
                Script::WaitForCancel => {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(SdCppError::GenerationCancelled)
                }
                Script::Fail => Err(SdCppError::Generation { detail: "boom".to_string() }),
            }
        }

        fn unload(&self) {
            self.unloads.fetch_add(1, Ordering::SeqCst);
        }

        fn is_worker_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
    }

    /// A leaked fake (the env holds a `'static` service, as the product's process-wide one).
    fn fake(script: Script) -> &'static FakeService {
        Box::leak(Box::new(FakeService { script, jobs: Mutex::new(Vec::new()), unloads: AtomicUsize::new(0), running: AtomicBool::new(false) }))
    }

    fn named_choice() -> SdCppChoice {
        SdCppChoice { build: None, device: ms_config::SdCppDeviceChoice::Named { name: "Vulkan1".to_string(), description: "GPU".to_string() }, threads: 4 }
    }

    fn env(service: &'static FakeService, no_ai: bool) -> SdCppRunEnv {
        SdCppRunEnv { service, read_choice: named_choice, availability: LocalAiAvailability::resolve(no_ai, Ok(()), Ok(()), Ok(())), resolve_build: |_| Some(SdCppBuild::Vulkan) }
    }

    fn input() -> SdCppRunInput {
        SdCppRunInput {
            files: ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::new() },
            request: GenerateRequest {
                prompt: "clean".to_string(),
                negative_prompt: String::new(),
                width: 4,
                height: 2,
                init_image: None,
                mask: None,
                references: Vec::new(),
                strength: 1.0,
                steps: 2,
                cfg_scale: 1.0,
                seed: 1,
                sample_method: None,
                scheduler: None,
                vae_tiling: false,
            },
            options: SdCppRunOptions { vae_tiling: true, vram_budget_gib: Some(0.5), flash_attention: FlashAttentionChoice::Off, ..SdCppRunOptions::default() },
        }
    }

    /// Polls `job` until it answers (bounded).
    fn wait(job: &mut SdCppRunJob) -> Result<egui::ColorImage, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(answer) = job.poll() {
                return answer;
            }
            assert!(Instant::now() < deadline, "the run never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A successful run builds the job from the choice and the normalized options (VAE tiling
    /// moved into the request), answers the image and leaves the bar inactive.
    #[test]
    fn a_run_builds_the_job_and_answers_the_image() {
        let service = fake(Script::Succeed);
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let mut job = SdCppRunJob::start_with(env(service, false), input(), &progress, "test");
        let image = wait(&mut job).expect("the fake succeeds");
        assert_eq!(image.size, [4, 2], "converted on the worker");
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(7, 7, 7));
        let jobs = service.jobs.lock().expect("fake lock");
        let sent = jobs.first().expect("one job was sent");
        assert_eq!(sent.build, SdCppBuild::Vulkan);
        assert!(sent.request.vae_tiling, "VAE tiling comes from the options");
        assert_eq!(sent.context.device, DeviceSelection::Named { name: "Vulkan1".to_string(), description: "GPU".to_string() });
        assert_eq!(sent.context.threads, Some(4));
        assert_eq!(sent.context.max_vram_gib, Some(1.0), "the budget is normalized");
        assert_eq!(sent.context.flash_attention, ms_sdcpp::FlashAttention::Off);
        let state = lock_progress(&progress);
        assert!(!state.active, "the bar ends with the run");
        assert_eq!((state.step, state.total), (2, 2), "the last step reached the bar");
    }

    /// `--no-ai` refuses before the service is asked.
    #[test]
    fn a_disabled_runtime_refuses_without_calling_the_service() {
        let service = fake(Script::Succeed);
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let mut job = SdCppRunJob::start_with(env(service, true), input(), &progress, "test");
        assert!(wait(&mut job).is_err());
        assert!(service.jobs.lock().expect("fake lock").is_empty());
        assert!(!lock_progress(&progress).active);
    }

    /// No build offered here: refused before the service is asked.
    #[test]
    fn no_build_refuses_without_calling_the_service() {
        let service = fake(Script::Succeed);
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let env = SdCppRunEnv { resolve_build: |_| None, ..env(service, false) };
        let mut job = SdCppRunJob::start_with(env, input(), &progress, "test");
        assert!(wait(&mut job).is_err());
        assert!(service.jobs.lock().expect("fake lock").is_empty());
    }

    /// A worker error reaches the engine as the error's localized text.
    #[test]
    fn a_service_error_is_reported_as_its_text() {
        let service = fake(Script::Fail);
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let mut job = SdCppRunJob::start_with(env(service, false), input(), &progress, "test");
        assert_eq!(wait(&mut job), Err(SdCppError::Generation { detail: "boom".to_string() }.to_string()));
    }

    /// Cancel raises the flag the service watches, retires the bar at once and keeps the
    /// abandoned run busy until its worker ends.
    #[test]
    fn cancel_raises_the_flag_retires_the_bar_and_parks_the_run() {
        let service = fake(Script::WaitForCancel);
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        let job = SdCppRunJob::start_with(env(service, false), input(), &progress, "test");
        let claimed = lock_progress(&progress).generation;
        let mut abandoned = AbandonedJobs::default();
        job.cancel(&mut abandoned, "test");
        {
            let state = lock_progress(&progress);
            assert!(!state.active, "the bar disappears at once");
            assert_ne!(state.generation, claimed, "the run's generation is retired");
        }
        assert!(abandoned.busy(), "the cancelled run still counts as busy");
        let deadline = Instant::now() + Duration::from_secs(10);
        while abandoned.poll() {
            assert!(Instant::now() < deadline, "the cancelled worker never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// An answer converts into an opaque `ColorImage` of its size.
    #[test]
    fn an_answer_becomes_a_color_image() {
        let rgb = RgbImage::new(2, 1, vec![1, 2, 3, 4, 5, 6]).expect("2x1");
        let image = rgb_to_color_image(&rgb).expect("fits");
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(4, 5, 6));
    }

    /// Dropping a job raises its cancel flag too.
    #[test]
    fn dropping_the_job_cancels_the_run() {
        let flag = Arc::new(AtomicBool::new(false));
        drop(CancelOnDrop(Arc::clone(&flag)));
        assert!(flag.load(Ordering::SeqCst));
    }

    /// Stage events show their text with no counter; step events count; a retired generation's
    /// events are dropped.
    #[test]
    fn run_events_map_onto_the_bar_under_the_generation_guard() {
        let progress = Mutex::new(EngineProgress::default());
        let claim = begin_progress_generation(&progress);
        publish_run_event(&progress, claim.generation, RunEvent::Stage(RunStage::LoadingModel));
        {
            let state = lock_progress(&progress);
            assert_eq!(state.phase, "generate");
            assert_eq!((state.step, state.total), (0, 0));
            assert_eq!(state.label, stage_text(RunStage::LoadingModel));
        }
        publish_run_event(&progress, claim.generation, RunEvent::Step { step: 3, steps: 8 });
        {
            let state = lock_progress(&progress);
            assert_eq!((state.step, state.total), (3, 8));
            assert_eq!(state.label, stage_text(RunStage::LoadingModel), "a step keeps the stage text");
        }
        publish_run_event(&progress, claim.generation.wrapping_add(1), RunEvent::Step { step: 7, steps: 8 });
        assert_eq!(lock_progress(&progress).step, 3, "a foreign generation's event is dropped");
    }

    /// The product service implements the seam (compile-time check); an unload is asked only
    /// while a worker runs.
    #[test]
    fn unload_is_asked_only_while_a_worker_runs() {
        fn assert_seam<T: SdCppGenerate>() {}
        assert_seam::<SdCppService>();
        let idle = fake(Script::Succeed);
        assert!(!unload_if_running(idle));
        assert_eq!(idle.unloads.load(Ordering::SeqCst), 0);
        let running = fake(Script::Succeed);
        running.running.store(true, Ordering::SeqCst);
        assert!(unload_if_running(running));
        assert_eq!(running.unloads.load(Ordering::SeqCst), 1);
    }
}
